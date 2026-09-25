package server

import (
	"context"
	"errors"
	"fmt"
	"net/http"
	"strconv"
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

// pagingSubClient serves its items in pages and records the limits it saw.
//
// dynamicfake ignores Limit and Continue, so the existing Subscribe test
// exercises the unpaged path whatever the server asks for: removing the
// continuation loop, or failing to forward the token, would still pass there.
type pagingSubClient struct {
	goneClient
	items  []unstructured.Unstructured
	limits []int64
	conts  []string
}

func (c *pagingSubClient) List(_ context.Context, _ schema.GroupVersionKind, _, _ string, limit int64, cont string) (*unstructured.UnstructuredList, error) {
	c.limits = append(c.limits, limit)
	c.conts = append(c.conts, cont)
	start := 0
	if cont != "" {
		n, err := strconv.Atoi(cont)
		if err != nil {
			return nil, fmt.Errorf("bad continue token %q", cont)
		}
		start = n
	}
	end := len(c.items)
	if limit > 0 && start+int(limit) < end {
		end = start + int(limit)
	}
	out := &unstructured.UnstructuredList{Items: append([]unstructured.Unstructured(nil), c.items[start:end]...)}
	// Every page carries the first page's snapshot, as the API server does.
	out.SetResourceVersion("100")
	if end < len(c.items) {
		out.SetContinue(strconv.Itoa(end))
	}
	return out, nil
}

func (c *pagingSubClient) Watch(ctx context.Context, _ schema.GroupVersionKind, _, _ string) (watch.Interface, error) {
	fw := watch.NewFake()
	go func() { <-ctx.Done(); fw.Stop() }()
	return fw, nil
}

// A paged initial listing delivers every object exactly once, then SYNCED.
func TestSubscribeListsEveryPageBeforeSyncing(t *testing.T) {
	t.Parallel()
	const total = defaultPageSize*2 + 7 // forces three pages
	items := make([]unstructured.Unstructured, total)
	for i := range items {
		items[i] = unstructured.Unstructured{Object: map[string]any{
			"apiVersion": "v1",
			"kind":       "Pod",
			"metadata": map[string]any{
				"name":      fmt.Sprintf("p-%d", i),
				"namespace": "default",
			},
		}}
	}
	c := &pagingSubClient{items: items}
	srv := New("t", nil, c, nil)

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	rec := newRecorder(ctx)
	done := make(chan error, 1)
	go func() { done <- srv.Subscribe(&axiomv1.SubscribeRequest{Gvk: podGVK, Namespace: "default"}, rec) }()

	seen := map[string]int{}
	var synced *axiomv1.SubscribeResponse
	for synced == nil {
		ev := rec.next(t)
		switch ev.GetType() {
		case axiomv1.SubscribeResponse_TYPE_ADDED:
			seen[ev.GetObject().GetName()]++
		case axiomv1.SubscribeResponse_TYPE_SYNCED:
			synced = ev
		default:
			t.Fatalf("unexpected event before SYNCED: %v", ev.GetType())
		}
	}

	if len(seen) != total {
		t.Errorf("saw %d distinct objects, want %d: a page was dropped", len(seen), total)
	}
	for name, n := range seen {
		if n != 1 {
			t.Errorf("%s delivered %d times, want once: a continuation repeated a page", name, n)
		}
	}
	// SYNCED carries the listing's resourceVersion, which is the first page's.
	if synced.GetResourceVersion() != "100" {
		t.Errorf("SYNCED resource_version = %q, want the listing's snapshot", synced.GetResourceVersion())
	}
	if len(c.limits) < 3 {
		t.Fatalf("expected at least 3 pages, saw %d calls", len(c.limits))
	}
	// The limit stays put across a continuation when every page fits: it only
	// changes when a page proves too large for one response.
	for i, l := range c.limits {
		if l != c.limits[0] {
			t.Errorf("limit changed mid-walk: call %d used %d, first used %d", i, l, c.limits[0])
		}
	}
	if c.conts[0] != "" {
		t.Errorf("first call carried a continue token %q", c.conts[0])
	}
	cancel()
	<-done
}

// localPagingSubClient is a pagingSubClient whose kind kube-apiserver serves
// itself, so an oversized continuation may be re-requested smaller.
type localPagingSubClient struct{ *pagingSubClient }

func (localPagingSubClient) ServedLocally(context.Context, schema.GroupVersion) bool { return true }

// #85 on the watch path: a watch-mode table's initial listing pages through the
// same byte-bounded fetch, so a later page larger than the first must shrink
// there too, and still deliver every object exactly once before SYNCED.
func TestSubscribeShrinksALaterPageThatOutgrowsTheFirst(t *testing.T) {
	t.Parallel()
	var items []unstructured.Unstructured
	for i := range defaultPageSize {
		items = append(items, padded(fmt.Sprintf("a-small-%03d", i), 8))
	}
	for i := range 4 {
		items = append(items, padded(fmt.Sprintf("b-large-%d", i), 3<<19))
	}
	c := localPagingSubClient{&pagingSubClient{items: items}}
	srv := New("t", nil, c, nil)

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	rec := newRecorder(ctx)
	done := make(chan error, 1)
	go func() {
		done <- srv.Subscribe(&axiomv1.SubscribeRequest{
			Gvk: &axiomv1.GroupVersionKind{Version: "v1", Kind: "ConfigMap"}, Namespace: "default",
		}, rec)
	}()

	seen := map[string]int{}
	for synced := false; !synced; {
		select {
		case err := <-done:
			t.Fatalf("#85: the initial listing failed before SYNCED: %v", err)
		default:
		}
		ev := rec.next(t)
		switch ev.GetType() {
		case axiomv1.SubscribeResponse_TYPE_ADDED:
			seen[ev.GetObject().GetName()]++
		case axiomv1.SubscribeResponse_TYPE_SYNCED:
			synced = true
		default:
			t.Fatalf("unexpected event before SYNCED: %v", ev.GetType())
		}
	}
	if len(seen) != len(items) {
		t.Errorf("saw %d distinct objects, want %d", len(seen), len(items))
	}
	for name, n := range seen {
		if n != 1 {
			t.Errorf("%s delivered %d times, want once", name, n)
		}
	}
	if len(c.limits) < 3 || c.limits[len(c.limits)-1] >= int64(defaultPageSize) {
		t.Errorf("limits were %v; the oversized continuation should have shrunk", c.limits)
	}
}
