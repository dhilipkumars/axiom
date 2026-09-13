package server

import (
	"context"
	"errors"
	"net/http"
	"testing"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/apis/meta/v1/unstructured"
	"k8s.io/apimachinery/pkg/runtime/schema"
	"k8s.io/apimachinery/pkg/watch"
	dynamicfake "k8s.io/client-go/dynamic/fake"
	"k8s.io/client-go/kubernetes/scheme"

	axiomv1 "github.com/dhilipkumars/axiom/gateway/gen/axiom/v1"
	"github.com/dhilipkumars/axiom/gateway/internal/k8s"
)

// recorder is a ServerStreamingServer that captures sent events.
type recorder struct {
	grpc.ServerStream
	ctx    context.Context
	events chan *axiomv1.SubscribeResponse
}

func newRecorder(ctx context.Context) *recorder {
	return &recorder{ctx: ctx, events: make(chan *axiomv1.SubscribeResponse, 64)}
}

func (r *recorder) Context() context.Context                 { return r.ctx }
func (r *recorder) Send(ev *axiomv1.SubscribeResponse) error { r.events <- ev; return nil }

func (r *recorder) next(t *testing.T) *axiomv1.SubscribeResponse {
	t.Helper()
	select {
	case ev := <-r.events:
		return ev
	case <-time.After(5 * time.Second):
		t.Fatal("timed out waiting for a watch event")
		return nil
	}
}

func TestSubscribeListsThenStreams(t *testing.T) {
	t.Parallel()
	dyn := dynamicfake.NewSimpleDynamicClient(scheme.Scheme,
		testPod("default", "web", "Running", "n1"),
		testPod("default", "db", "Pending", ""),
		testPod("other", "x", "Running", "n2"))
	client := k8s.NewDynamic(dyn, k8s.NewStaticMapper(k8s.BuiltinKinds()...))
	srv := New("t", nil, client, nil)
	ctx, cancel := context.WithCancel(context.Background())
	rec := newRecorder(ctx)
	done := make(chan error, 1)
	go func() { done <- srv.Subscribe(&axiomv1.SubscribeRequest{Gvk: podGVK, Namespace: "default"}, rec) }()

	// Initial listing: two ADDED (namespace-scoped) with no stream RV, then SYNCED.
	seen := map[string]bool{}
	for i := 0; i < 2; i++ {
		ev := rec.next(t)
		if ev.GetType() != axiomv1.SubscribeResponse_TYPE_ADDED {
			t.Fatalf("event %d type = %v, want ADDED", i, ev.GetType())
		}
		if ev.GetResourceVersion() != "" {
			t.Fatalf("initial ADDED must not carry a resume RV, got %q", ev.GetResourceVersion())
		}
		seen[ev.GetObject().GetName()] = true
	}
	if !seen["web"] || !seen["db"] || len(seen) != 2 {
		t.Fatalf("initial listing = %v", seen)
	}
	if ev := rec.next(t); ev.GetType() != axiomv1.SubscribeResponse_TYPE_SYNCED {
		t.Fatalf("expected SYNCED, got %v", ev.GetType())
	}

	// Live events.
	u := &unstructured.Unstructured{}
	u.SetAPIVersion("v1")
	u.SetKind("Pod")
	u.SetNamespace("default")
	u.SetName("new")
	if _, err := client.Create(ctx, schema.GroupVersionKind{Version: "v1", Kind: "Pod"}, "default", u); err != nil {
		t.Fatal(err)
	}
	if ev := rec.next(t); ev.GetType() != axiomv1.SubscribeResponse_TYPE_ADDED || ev.GetObject().GetName() != "new" {
		t.Fatalf("expected ADDED new, got %v %q", ev.GetType(), ev.GetObject().GetName())
	}
	if err := client.Delete(ctx, schema.GroupVersionKind{Version: "v1", Kind: "Pod"}, "default", "new"); err != nil {
		t.Fatal(err)
	}
	if ev := rec.next(t); ev.GetType() != axiomv1.SubscribeResponse_TYPE_DELETED || ev.GetObject().GetName() != "new" {
		t.Fatalf("expected DELETED new, got %v %q", ev.GetType(), ev.GetObject().GetName())
	}

	cancel()
	select {
	case err := <-done:
		if err != nil {
			t.Fatalf("Subscribe returned %v after cancel, want nil", err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("Subscribe did not return after cancel")
	}
}

func TestSubscribeValidationAndNoCluster(t *testing.T) {
	t.Parallel()
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	srv := New("t", nil, nil, nil)
	cases := []struct {
		name string
		req  *axiomv1.SubscribeRequest
		want codes.Code
	}{
		{name: "nil", req: nil, want: codes.InvalidArgument},
		{name: "missing gvk", req: &axiomv1.SubscribeRequest{}, want: codes.InvalidArgument},
		{name: "bad namespace", req: &axiomv1.SubscribeRequest{Gvk: podGVK, Namespace: "Bad"}, want: codes.InvalidArgument},
		{name: "no cluster", req: &axiomv1.SubscribeRequest{Gvk: podGVK}, want: codes.FailedPrecondition},
	}
	for _, tc := range cases {
		err := srv.Subscribe(tc.req, newRecorder(ctx))
		if got := status.Code(err); got != tc.want {
			t.Fatalf("%s: code = %v, want %v (err=%v)", tc.name, got, tc.want, err)
		}
	}
	err := New("t", nil, errClient{err: apierrors.NewForbidden(schema.GroupResource{Resource: "pods"}, "", errors.New("rbac"))}, nil).
		Subscribe(&axiomv1.SubscribeRequest{Gvk: podGVK}, newRecorder(ctx))
	if status.Code(err) != codes.PermissionDenied {
		t.Fatalf("forbidden list code = %v", status.Code(err))
	}
}

// goneClient serves an empty list, then a watch that immediately reports 410.
type goneClient struct {
	errClient
	viaWatchError bool
}

func (g goneClient) List(context.Context, schema.GroupVersionKind, string, string, int64, string) (*unstructured.UnstructuredList, error) {
	l := &unstructured.UnstructuredList{}
	l.SetResourceVersion("100")
	return l, nil
}

func (g goneClient) Watch(context.Context, schema.GroupVersionKind, string, string) (watch.Interface, error) {
	if !g.viaWatchError {
		return nil, apierrors.NewResourceExpired("too old")
	}
	fw := watch.NewFake()
	go fw.Error(&metav1.Status{Code: http.StatusGone, Reason: metav1.StatusReasonExpired, Message: "too old"})
	return fw, nil
}

func TestSubscribeResyncRequiredOn410(t *testing.T) {
	t.Parallel()
	for _, viaWatch := range []bool{false, true} {
		ctx, cancel := context.WithCancel(context.Background())
		rec := newRecorder(ctx)
		err := New("t", nil, goneClient{viaWatchError: viaWatch}, nil).Subscribe(&axiomv1.SubscribeRequest{Gvk: podGVK}, rec)
		if err != nil {
			t.Fatalf("viaWatch=%v: Subscribe returned %v, want nil after RESYNC_REQUIRED", viaWatch, err)
		}
		if ev := rec.next(t); ev.GetType() != axiomv1.SubscribeResponse_TYPE_SYNCED || ev.GetResourceVersion() != "100" {
			t.Fatalf("viaWatch=%v: first event = %v rv=%q, want SYNCED 100", viaWatch, ev.GetType(), ev.GetResourceVersion())
		}
		if ev := rec.next(t); ev.GetType() != axiomv1.SubscribeResponse_TYPE_RESYNC_REQUIRED {
			t.Fatalf("viaWatch=%v: expected RESYNC_REQUIRED, got %v", viaWatch, ev.GetType())
		}
		cancel()
	}
}

// Resuming with a resource_version must not list.
type countingClient struct {
	goneClient
	lists int
}

func (c *countingClient) List(context.Context, schema.GroupVersionKind, string, string, int64, string) (*unstructured.UnstructuredList, error) {
	c.lists++
	l := &unstructured.UnstructuredList{}
	l.SetResourceVersion("100")
	return l, nil
}

func (c *countingClient) Watch(ctx context.Context, _ schema.GroupVersionKind, _, _ string) (watch.Interface, error) {
	fw := watch.NewFake()
	go func() { <-ctx.Done(); fw.Stop() }()
	return fw, nil
}

func TestSubscribeWithResourceVersionSkipsList(t *testing.T) {
	t.Parallel()
	c := &countingClient{}
	ctx, cancel := context.WithCancel(context.Background())
	rec := newRecorder(ctx)
	done := make(chan error, 1)
	go func() {
		done <- New("t", nil, c, nil).Subscribe(&axiomv1.SubscribeRequest{Gvk: podGVK, ResourceVersion: "42"}, rec)
	}()
	time.Sleep(100 * time.Millisecond)
	cancel()
	if err := <-done; err != nil {
		t.Fatal(err)
	}
	if c.lists != 0 {
		t.Fatalf("resume issued %d LISTs, want 0", c.lists)
	}
	// No listing and no SYNCED on resume: only backlog/live events would follow.
	select {
	case ev := <-rec.events:
		t.Fatalf("unexpected event on resume: %v", ev.GetType())
	default:
	}
}
