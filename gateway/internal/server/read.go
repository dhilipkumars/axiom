package server

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"regexp"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	"k8s.io/apimachinery/pkg/apis/meta/v1/unstructured"
	"k8s.io/apimachinery/pkg/runtime/schema"

	axiomv1 "github.com/dhilipkumars/axiom/gateway/gen/axiom/v1"
	"github.com/dhilipkumars/axiom/gateway/internal/k8s"
)

// dns1123Subdomain matches valid Kubernetes object names and namespaces.
// Checked before anything reaches client-go so a caller can never smuggle
// path segments or selectors through these fields (docs/RULES.md §3).
var dns1123Subdomain = regexp.MustCompile(`^[a-z0-9]([-a-z0-9]*[a-z0-9])?(\.[a-z0-9]([-a-z0-9]*[a-z0-9])?)*$`)

const maxNameLen = 253

// gvkFromProto validates and converts the wire GVK.
func gvkFromProto(g *axiomv1.GroupVersionKind) (schema.GroupVersionKind, error) {
	if g == nil || g.GetVersion() == "" || g.GetKind() == "" {
		return schema.GroupVersionKind{}, status.Error(codes.InvalidArgument, "gvk: version and kind are required")
	}
	return schema.GroupVersionKind{Group: g.GetGroup(), Version: g.GetVersion(), Kind: g.GetKind()}, nil
}

// validName checks a namespace or name. Empty is accepted (callers decide
// whether empty is allowed for the field in question).
func validName(field, v string) error {
	if v == "" {
		return nil
	}
	if len(v) > maxNameLen || !dns1123Subdomain.MatchString(v) {
		return status.Errorf(codes.InvalidArgument, "%s %q is not a valid DNS-1123 subdomain", field, v)
	}
	return nil
}

// toGRPC maps k8s/client errors onto gRPC status codes. The message never
// includes request headers or credentials: apimachinery StatusError messages
// describe the resource, and client-go transport errors describe the endpoint.
// Paging bounds, in objects and in bytes.
//
// defaultPageSize is what a caller asking for nothing gets. It is a
// compromise: too small multiplies WAN round trips across a scan, too large
// re-creates the unbounded response this exists to prevent. 500 is the same
// order client-go's own reflectors use.
//
// maxPageSize is the ceiling a caller can ask for, so a client cannot demand a
// response the gateway would fail to send.
//
// maxPageBytes bounds the JSON in one response and is deliberately well under
// MaxMessageBytes: the message carries protobuf framing and per-object
// metadata on top of the JSON, and the margin means a page that fits the
// budget always fits the message.
const (
	defaultPageSize = 200
	// A caller cannot raise the first allocation without bound. The byte
	// budget below is measured only after client-go has decoded the page, so
	// the object count is the only thing standing between a request and the
	// memory it costs to answer: 1000 ConfigMaps of a megabyte each is a
	// gigabyte materialised before a single byte is counted. Shrinking after
	// the fact protects the response, not the fetch; this protects the fetch.
	maxPageSize  = 1000
	maxPageBytes = 4 << 20
	// MaxMessageBytes is the gRPC message limit both ends must agree on. The
	// 4 MiB default is reachable on an ordinary cluster: a few hundred Pods
	// carrying managedFields will do it. Exported so the server sets the same
	// number it documents, and so the extension's matching constant has one
	// place to be compared against.
	MaxMessageBytes = 16 << 20
)

// clampLimit turns a caller's requested page size into one the gateway will
// serve. Zero (or negative, which the wire type permits) means "choose for me".
func clampLimit(requested int32) int32 {
	switch {
	case requested <= 0:
		return defaultPageSize
	case requested > maxPageSize:
		return maxPageSize
	default:
		return requested
	}
}

// fetchBoundedPage reads one page, shrinking it until its JSON fits the byte
// budget, and returns the encoded objects alongside the raw list.
//
// The limit bounds objects, not bytes, and objects vary by three orders of
// magnitude: a count that is comfortable for Pods can be hundreds of megabytes
// of ConfigMaps holding a megabyte each. A Kubernetes continue token is opaque
// and points at a page boundary, so a page cannot be split after the fact --
// but the token that produced it is still valid, so asking again for half as
// many returns the same objects from the same snapshot. Nothing has reached
// the caller at that point.
//
// What this does not do is bound the *fetch*: client-go decodes the whole
// requested page before its size can be measured. maxPageSize is what bounds
// that, which is why it is modest.
func (s *Server) fetchBoundedPage(
	ctx context.Context,
	gvk schema.GroupVersionKind,
	namespace, name string,
	limit int32,
	continueToken string,
) ([]*axiomv1.Object, *unstructured.UnstructuredList, int, error) {
	for {
		list, err := s.k8s.List(ctx, gvk, namespace, name, int64(limit), continueToken)
		if err != nil {
			return nil, nil, 0, toGRPC(err)
		}
		objs := make([]*axiomv1.Object, 0, len(list.Items))
		size := 0
		for i := range list.Items {
			po, err := objectToProto(&list.Items[i])
			if err != nil {
				return nil, nil, 0, err
			}
			size += len(po.GetJson())
			objs = append(objs, po)
		}
		if size <= maxPageBytes || limit <= 1 {
			return objs, list, size, nil
		}
		// Round up, so 3 becomes 2 rather than 1. Halving downwards
		// overshoots on small limits and buys an extra round trip for a page
		// that would have fit. Still strictly decreasing while limit > 1, so
		// the loop terminates.
		limit = (limit + 1) / 2
		s.log.LogAttrs(ctx, slog.LevelInfo, "list_page_shrunk",
			slog.String("gvk", gvk.String()),
			slog.Int("bytes", size),
			slog.Int("new_limit", int(limit)))
	}
}

func toGRPC(err error) error {
	switch {
	case err == nil:
		return nil
	case errors.Is(err, k8s.ErrNoCluster):
		return status.Error(codes.FailedPrecondition, err.Error())
	case errors.Is(err, k8s.ErrUnsupportedKind):
		return status.Error(codes.InvalidArgument, err.Error())
	case apierrors.IsNotFound(err):
		return status.Error(codes.NotFound, err.Error())
	case apierrors.IsConflict(err):
		// Stale resourceVersion on Update: distinct so callers can re-read and retry.
		return status.Error(codes.Aborted, err.Error())
	case apierrors.IsResourceExpired(err), apierrors.IsGone(err):
		// A continue token whose snapshot the API server has compacted away.
		// ABORTED, the same code as a write conflict, because the caller's
		// recourse is identical: start again. It must not be retried
		// transparently here -- earlier pages have already gone to the SQL
		// executor, so resuming from scratch would duplicate rows, and
		// docs/RULES.md §1 forbids papering over it.
		return status.Error(codes.Aborted, fmt.Sprintf("list continuation expired: %v", err))
	case apierrors.IsAlreadyExists(err):
		return status.Error(codes.AlreadyExists, err.Error())
	case apierrors.IsForbidden(err), apierrors.IsUnauthorized(err):
		return status.Error(codes.PermissionDenied, err.Error())
	case apierrors.IsInvalid(err), apierrors.IsBadRequest(err):
		return status.Error(codes.InvalidArgument, err.Error())
	case apierrors.IsTimeout(err), apierrors.IsServerTimeout(err), apierrors.IsServiceUnavailable(err), apierrors.IsTooManyRequests(err):
		return status.Error(codes.Unavailable, err.Error())
	case errors.Is(err, context.DeadlineExceeded):
		return status.Error(codes.DeadlineExceeded, err.Error())
	default:
		// Transport-level failures (connection refused, DNS, TLS) arrive as
		// plain errors from client-go; the API server is unreachable.
		if _, isStatus := err.(apierrors.APIStatus); !isStatus {
			return status.Error(codes.Unavailable, fmt.Sprintf("kubernetes api server unreachable: %v", err))
		}
		return status.Error(codes.Internal, err.Error())
	}
}

// objectToProto serialises one object. Serialisation failure is Internal:
// the API server handed us something we cannot re-encode.
func objectToProto(u *unstructured.Unstructured) (*axiomv1.Object, error) {
	raw, err := u.MarshalJSON()
	if err != nil {
		return nil, status.Errorf(codes.Internal, "marshal %s/%s: %v", u.GetNamespace(), u.GetName(), err)
	}
	return &axiomv1.Object{
		Namespace:       u.GetNamespace(),
		Name:            u.GetName(),
		ResourceVersion: u.GetResourceVersion(),
		Json:            raw,
	}, nil
}

// Get fetches one object from the API server.
//
// Contract: see axiom.proto. Validates gvk/namespace/name before any cluster
// call; name is required. Errors are mapped by toGRPC. No side effects.
func (s *Server) Get(ctx context.Context, req *axiomv1.GetRequest) (*axiomv1.GetResponse, error) {
	s.getCalls.Add(1)
	if req == nil {
		return nil, status.Error(codes.InvalidArgument, "get: request must not be nil")
	}
	gvk, err := gvkFromProto(req.GetGvk())
	if err != nil {
		return nil, err
	}
	if req.GetName() == "" {
		return nil, status.Error(codes.InvalidArgument, "get: name is required")
	}
	if err := validName("namespace", req.GetNamespace()); err != nil {
		return nil, err
	}
	if err := validName("name", req.GetName()); err != nil {
		return nil, err
	}
	obj, err := s.k8s.Get(ctx, gvk, req.GetNamespace(), req.GetName())
	if err != nil {
		return nil, toGRPC(err)
	}
	po, err := objectToProto(obj)
	if err != nil {
		return nil, err
	}
	return &axiomv1.GetResponse{Object: po}, nil
}

// List fetches objects of one kind, narrowed server-side by namespace and/or
// name.
//
// Contract: see axiom.proto. Validates inputs before any cluster call. Logs
// one line per call with the filters applied so pushdown is observable
// (the Phase 1 E2E asserts on it). No side effects.
func (s *Server) List(ctx context.Context, req *axiomv1.ListRequest) (*axiomv1.ListResponse, error) {
	s.listCalls.Add(1)
	if req == nil {
		return nil, status.Error(codes.InvalidArgument, "list: request must not be nil")
	}
	gvk, err := gvkFromProto(req.GetGvk())
	if err != nil {
		return nil, err
	}
	if err := validName("namespace", req.GetNamespace()); err != nil {
		return nil, err
	}
	if err := validName("name", req.GetName()); err != nil {
		return nil, err
	}
	objs, list, size, err := s.fetchBoundedPage(ctx, gvk, req.GetNamespace(), req.GetName(),
		clampLimit(req.GetLimit()), req.GetContinueToken())
	if err != nil {
		return nil, err
	}
	// A single object over the budget cannot be paged around. Say so, rather
	// than letting the transport reject the message with a size error that
	// names nothing (docs/RULES.md §1).
	if size > maxPageBytes && len(objs) == 1 {
		return nil, status.Errorf(codes.ResourceExhausted,
			"a single %s object is %d bytes, over the %d byte limit for one response; "+
				"paging cannot split one object", gvk.Kind, size, maxPageBytes)
	}
	resp := &axiomv1.ListResponse{
		Objects:         objs,
		ResourceVersion: list.GetResourceVersion(),
		ContinueToken:   list.GetContinue(),
	}
	s.log.LogAttrs(ctx, slog.LevelInfo, "list",
		slog.String("gvk", gvk.String()),
		slog.String("namespace", req.GetNamespace()),
		slog.String("name", req.GetName()),
		slog.Int("count", len(resp.Objects)))
	return resp, nil
}
