package server

import (
	"context"
	"errors"
	"log/slog"
	"net/http"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/apis/meta/v1/unstructured"
	"k8s.io/apimachinery/pkg/watch"

	axiomv1 "github.com/dhilipkumars/axiom/gateway/gen/axiom/v1"
)

// Subscribe streams current state and live changes for one kind/namespace.
//
// Contract: see axiom.proto. This is a raw list+watch with resume-from-bookmark
// (not a client-go informer: an informer's cache would duplicate the
// extension's, and its resync semantics hide the RV bookkeeping the extension
// needs; docs/PLAN.md records this scope change). The initial LIST (when no
// resource_version is given) is served straight from the API server and
// logged as `subscribe_list`, so tests can assert that cached reads issue no
// further LISTs. Transient watch closes are re-watched from the last seen
// resourceVersion; a 410 Gone ends the stream after a TYPE_RESYNC_REQUIRED
// event. A resume sends no SYNCED; the API server's first BOOKMARK tells the
// caller it is current again.
//
// TODO(phase8): de-duplicate upstream watches across subscribers to the same
// (gvk, namespace) with a shared informer factory (docs/DESIGN.md §8).
func (s *Server) Subscribe(req *axiomv1.SubscribeRequest, stream axiomv1.GatewayService_SubscribeServer) error {
	s.subscribeCalls.Add(1)
	if req == nil {
		return status.Error(codes.InvalidArgument, "subscribe: request must not be nil")
	}
	gvk, err := gvkFromProto(req.GetGvk())
	if err != nil {
		return err
	}
	if err := validName("namespace", req.GetNamespace()); err != nil {
		return err
	}
	ctx := stream.Context()
	ns := req.GetNamespace()
	rv := req.GetResourceVersion()
	logAttrs := []slog.Attr{slog.String("gvk", gvk.String()), slog.String("namespace", ns)}
	s.log.LogAttrs(ctx, slog.LevelInfo, "subscribe", append(logAttrs, slog.String("resource_version", rv))...)

	send := func(t axiomv1.SubscribeResponse_Type, obj *unstructured.Unstructured, rv string) error {
		ev := &axiomv1.SubscribeResponse{Type: t, ResourceVersion: rv}
		if obj != nil {
			po, err := objectToProto(obj)
			if err != nil {
				return err
			}
			ev.Object = po
		}
		return stream.Send(ev)
	}

	if rv == "" {
		s.subscribeListCalls.Add(1)
		// Paged, not one call. An unpaged listing pulls the whole collection
		// into gateway memory before a single event goes out, which on a large
		// kind is both a memory spike and a long silence before SYNCED.
		//
		// Every page after the first carries the first page's snapshot, so the
		// listing is consistent even though it arrives in pieces. That is what
		// makes the resourceVersion of the *first* page the correct watch start
		// point, and why it is captured rather than taken from the last page.
		var (
			count int
			token string
		)
		// One limit for the whole walk. A continue token may only be used
		// with identical query parameters apart from continue itself (see
		// ListOptions.Continue), so the size is settled on the first page and
		// then carried, rather than renegotiated per page.
		pageLimit := int32(defaultPageSize)
		for {
			// The same byte-bounded fetch the unary List uses. Paging by count
			// alone would leave the memory spike in place for exactly the
			// kinds that cause it: 200 ConfigMaps of a megabyte each is 200
			// MiB materialised before the first event goes out.
			// Only the first page may shrink; after that the token fixes it.
			_, list, _, effective, err := s.fetchBoundedPage(ctx, gvk, ns, "", pageLimit, token, token == "")
			if err != nil {
				return err
			}
			for i := range list.Items {
				// Empty stream RV: a partially delivered listing is not a resume point.
				if err := send(axiomv1.SubscribeResponse_TYPE_ADDED, &list.Items[i], ""); err != nil {
					return err
				}
			}
			pageLimit = effective
			count += len(list.Items)
			if rv == "" {
				rv = list.GetResourceVersion()
			}
			token = list.GetContinue()
			if token == "" {
				break
			}
		}
		s.log.LogAttrs(ctx, slog.LevelInfo, "subscribe_list", append(logAttrs, slog.Int("count", count), slog.String("resource_version", rv))...)
		if err := send(axiomv1.SubscribeResponse_TYPE_SYNCED, nil, rv); err != nil {
			return err
		}
	}

	for {
		w, err := s.k8s.Watch(ctx, gvk, ns, rv)
		if err != nil {
			if isGone(err) {
				s.log.LogAttrs(ctx, slog.LevelWarn, "subscribe_resync_required", append(logAttrs, slog.String("resource_version", rv))...)
				return send(axiomv1.SubscribeResponse_TYPE_RESYNC_REQUIRED, nil, rv)
			}
			if ctx.Err() != nil {
				return nil
			}
			return toGRPC(err)
		}
		next, err := s.pumpWatch(ctx, w, rv, send)
		w.Stop()
		if err != nil {
			if errors.Is(err, errGone) {
				s.log.LogAttrs(ctx, slog.LevelWarn, "subscribe_resync_required", append(logAttrs, slog.String("resource_version", next))...)
				return send(axiomv1.SubscribeResponse_TYPE_RESYNC_REQUIRED, nil, next)
			}
			return err
		}
		if ctx.Err() != nil {
			s.log.LogAttrs(ctx, slog.LevelInfo, "subscribe_end", append(logAttrs, slog.String("reason", "client cancelled"))...)
			return nil
		}
		// Watch closed by the server (timeout); re-watch from where we are.
		s.log.LogAttrs(ctx, slog.LevelDebug, "subscribe_rewatch", append(logAttrs, slog.String("resource_version", next))...)
		rv = next
	}
}

// errGone signals a 410 Gone seen inside the watch stream.
var errGone = errors.New("watch: resource version expired")

// isGone reports whether err is the API server saying the resourceVersion is too old.
func isGone(err error) bool {
	return apierrors.IsGone(err) || apierrors.IsResourceExpired(err)
}

// pumpWatch forwards watch events until the channel closes or ctx ends.
// Returns the last seen resourceVersion. A 410 carried as a watch Error event
// is returned as errGone.
func (s *Server) pumpWatch(
	ctx context.Context,
	w watch.Interface,
	rv string,
	send func(axiomv1.SubscribeResponse_Type, *unstructured.Unstructured, string) error,
) (string, error) {
	for {
		select {
		case <-ctx.Done():
			return rv, nil
		case ev, ok := <-w.ResultChan():
			if !ok {
				return rv, nil
			}
			switch ev.Type {
			case watch.Added, watch.Modified, watch.Deleted:
				obj, ok := ev.Object.(*unstructured.Unstructured)
				if !ok {
					return rv, status.Errorf(codes.Internal, "watch event carries %T, not an unstructured object", ev.Object)
				}
				if v := obj.GetResourceVersion(); v != "" {
					rv = v
				}
				t := map[watch.EventType]axiomv1.SubscribeResponse_Type{
					watch.Added:    axiomv1.SubscribeResponse_TYPE_ADDED,
					watch.Modified: axiomv1.SubscribeResponse_TYPE_MODIFIED,
					watch.Deleted:  axiomv1.SubscribeResponse_TYPE_DELETED,
				}[ev.Type]
				if err := send(t, obj, rv); err != nil {
					return rv, err
				}
			case watch.Bookmark:
				if obj, ok := ev.Object.(*unstructured.Unstructured); ok && obj.GetResourceVersion() != "" {
					rv = obj.GetResourceVersion()
				}
				if err := send(axiomv1.SubscribeResponse_TYPE_BOOKMARK, nil, rv); err != nil {
					return rv, err
				}
			case watch.Error:
				st, _ := ev.Object.(*metav1.Status)
				if st != nil && (st.Code == http.StatusGone || st.Reason == metav1.StatusReasonExpired) {
					return rv, errGone
				}
				msg := "watch error"
				if st != nil {
					msg = st.Message
				}
				return rv, status.Error(codes.Unavailable, "kubernetes watch failed: "+msg)
			}
		}
	}
}
