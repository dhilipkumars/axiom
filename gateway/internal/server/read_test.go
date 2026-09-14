package server

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"strconv"
	"strings"
	"testing"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	corev1 "k8s.io/api/core/v1"
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

var podGVK = &axiomv1.GroupVersionKind{Version: "v1", Kind: "Pod"}

func testPod(ns, name, phase, node string) *corev1.Pod {
	return &corev1.Pod{
		TypeMeta:   metav1.TypeMeta{APIVersion: "v1", Kind: "Pod"},
		ObjectMeta: metav1.ObjectMeta{Namespace: ns, Name: name, ResourceVersion: "7"},
		Spec:       corev1.PodSpec{NodeName: node},
		Status:     corev1.PodStatus{Phase: corev1.PodPhase(phase)},
	}
}

func newReadServer(t *testing.T) *Server {
	t.Helper()
	dyn := dynamicfake.NewSimpleDynamicClient(scheme.Scheme,
		testPod("default", "web", "Running", "node-a"),
		testPod("default", "db", "Pending", ""),
		testPod("kube-system", "dns", "Running", "node-b"))
	return New("t", nil, k8s.NewDynamic(dyn, k8s.NewStaticMapper(k8s.BuiltinKinds()...)), nil)
}

func TestGet(t *testing.T) {
	t.Parallel()
	srv := newReadServer(t)
	tests := []struct {
		name     string
		req      *axiomv1.GetRequest
		wantCode codes.Code
	}{
		{name: "found", req: &axiomv1.GetRequest{Gvk: podGVK, Namespace: "default", Name: "web"}, wantCode: codes.OK},
		{name: "not found", req: &axiomv1.GetRequest{Gvk: podGVK, Namespace: "default", Name: "nope"}, wantCode: codes.NotFound},
		{name: "nil request", req: nil, wantCode: codes.InvalidArgument},
		{name: "missing gvk", req: &axiomv1.GetRequest{Namespace: "default", Name: "web"}, wantCode: codes.InvalidArgument},
		{name: "unsupported kind", req: &axiomv1.GetRequest{Gvk: &axiomv1.GroupVersionKind{Group: "apps", Version: "v1", Kind: "Deployment"}, Namespace: "default", Name: "x"}, wantCode: codes.InvalidArgument},
		{name: "missing name", req: &axiomv1.GetRequest{Gvk: podGVK, Namespace: "default"}, wantCode: codes.InvalidArgument},
		{name: "bad name", req: &axiomv1.GetRequest{Gvk: podGVK, Namespace: "default", Name: "Web/../x"}, wantCode: codes.InvalidArgument},
		{name: "bad namespace", req: &axiomv1.GetRequest{Gvk: podGVK, Namespace: "def ault", Name: "web"}, wantCode: codes.InvalidArgument},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			resp, err := srv.Get(context.Background(), tc.req)
			if got := status.Code(err); got != tc.wantCode {
				t.Fatalf("code = %v, want %v (err=%v)", got, tc.wantCode, err)
			}
			if tc.wantCode != codes.OK {
				return
			}
			obj := resp.GetObject()
			if obj.GetName() != "web" || obj.GetNamespace() != "default" || obj.GetResourceVersion() != "7" {
				t.Fatalf("object meta = %+v", obj)
			}
			var m map[string]any
			if err := json.Unmarshal(obj.GetJson(), &m); err != nil {
				t.Fatalf("json: %v", err)
			}
			if m["kind"] != "Pod" {
				t.Fatalf("json kind = %v", m["kind"])
			}
		})
	}
}

func TestList(t *testing.T) {
	t.Parallel()
	srv := newReadServer(t)
	tests := []struct {
		name      string
		req       *axiomv1.ListRequest
		wantCode  codes.Code
		wantNames map[string]bool
	}{
		{name: "all", req: &axiomv1.ListRequest{Gvk: podGVK}, wantNames: map[string]bool{"web": true, "db": true, "dns": true}},
		{name: "namespace", req: &axiomv1.ListRequest{Gvk: podGVK, Namespace: "default"}, wantNames: map[string]bool{"web": true, "db": true}},
		// A name filter is a metadata.name field selector, which client-go's
		// fake dynamic client ignores, so the handler tests cover validation
		// and error mapping for it rather than the filtering itself.
		// TestListByNameSendsAFieldSelectorAndNeverAGet asserts the request the
		// gateway builds, and e2e/cluster_test.sh covers the result against a
		// real API server.
		{name: "nil request", req: nil, wantCode: codes.InvalidArgument},
		{name: "missing gvk", req: &axiomv1.ListRequest{}, wantCode: codes.InvalidArgument},
		{name: "bad namespace", req: &axiomv1.ListRequest{Gvk: podGVK, Namespace: "UPPER"}, wantCode: codes.InvalidArgument},
		{name: "bad name", req: &axiomv1.ListRequest{Gvk: podGVK, Name: "a_b"}, wantCode: codes.InvalidArgument},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			resp, err := srv.List(context.Background(), tc.req)
			if got := status.Code(err); got != tc.wantCode {
				t.Fatalf("code = %v, want %v (err=%v)", got, tc.wantCode, err)
			}
			if tc.wantCode != codes.OK {
				return
			}
			if len(resp.GetObjects()) != len(tc.wantNames) {
				t.Fatalf("got %d objects, want %d", len(resp.GetObjects()), len(tc.wantNames))
			}
			for _, o := range resp.GetObjects() {
				if !tc.wantNames[o.GetName()] {
					t.Fatalf("unexpected object %q", o.GetName())
				}
				if len(o.GetJson()) == 0 {
					t.Fatalf("object %q has empty json", o.GetName())
				}
			}
		})
	}
}

func TestReadsWithoutCluster(t *testing.T) {
	t.Parallel()
	srv := New("t", nil, nil, nil) // defaults to k8s.Unconfigured
	_, err := srv.Get(context.Background(), &axiomv1.GetRequest{Gvk: podGVK, Namespace: "d", Name: "a"})
	if status.Code(err) != codes.FailedPrecondition {
		t.Fatalf("Get code = %v, want FailedPrecondition", status.Code(err))
	}
	_, err = srv.List(context.Background(), &axiomv1.ListRequest{Gvk: podGVK})
	if status.Code(err) != codes.FailedPrecondition {
		t.Fatalf("List code = %v, want FailedPrecondition", status.Code(err))
	}
}

// errClient returns a fixed error from every call, for exercising toGRPC.
type errClient struct{ err error }

func (e errClient) Get(context.Context, schema.GroupVersionKind, string, string) (*unstructured.Unstructured, error) {
	return nil, e.err
}

func (e errClient) List(context.Context, schema.GroupVersionKind, string, string, int64, string) (*unstructured.UnstructuredList, error) {
	return nil, e.err
}

func (e errClient) Create(context.Context, schema.GroupVersionKind, string, *unstructured.Unstructured) (*unstructured.Unstructured, error) {
	return nil, e.err
}

func (e errClient) Update(context.Context, schema.GroupVersionKind, string, *unstructured.Unstructured) (*unstructured.Unstructured, error) {
	return nil, e.err
}

func (e errClient) Delete(context.Context, schema.GroupVersionKind, string, string) error {
	return e.err
}

func (e errClient) Watch(context.Context, schema.GroupVersionKind, string, string) (watch.Interface, error) {
	return nil, e.err
}

func (e errClient) Resolve(context.Context, schema.GroupVersionKind) (schema.GroupVersionResource, bool, error) {
	return schema.GroupVersionResource{}, false, e.err
}

func (e errClient) Describe(context.Context, schema.GroupVersionKind) (k8s.KindInfo, error) {
	return k8s.KindInfo{}, e.err
}

func (e errClient) Kinds(context.Context, *string, []string) ([]k8s.KindInfo, error) {
	return nil, e.err
}

func TestErrorMapping(t *testing.T) {
	t.Parallel()
	gr := schema.GroupResource{Resource: "pods"}
	tests := []struct {
		name string
		err  error
		want codes.Code
	}{
		{name: "forbidden", err: apierrors.NewForbidden(gr, "x", errors.New("rbac")), want: codes.PermissionDenied},
		{name: "unauthorized", err: apierrors.NewUnauthorized("token"), want: codes.PermissionDenied},
		{name: "not found", err: apierrors.NewNotFound(gr, "x"), want: codes.NotFound},
		{name: "conflict is Aborted", err: apierrors.NewConflict(gr, "x", errors.New("rv stale")), want: codes.Aborted},
		{name: "already exists", err: apierrors.NewAlreadyExists(gr, "x"), want: codes.AlreadyExists},
		{name: "bad request", err: apierrors.NewBadRequest("bad"), want: codes.InvalidArgument},
		{name: "server timeout", err: apierrors.NewServerTimeout(gr, "list", 1), want: codes.Unavailable},
		{name: "service unavailable", err: apierrors.NewServiceUnavailable("down"), want: codes.Unavailable},
		{name: "too many requests", err: apierrors.NewTooManyRequests("slow", 1), want: codes.Unavailable},
		{name: "generic status", err: apierrors.NewGenericServerResponse(http.StatusTeapot, "get", gr, "x", "teapot", 0, false), want: codes.Internal},
		{name: "transport error", err: errors.New("dial tcp 10.0.0.1:6443: connection refused"), want: codes.Unavailable},
		{name: "deadline", err: context.DeadlineExceeded, want: codes.DeadlineExceeded},
		{name: "unsupported kind", err: k8s.ErrUnsupportedKind, want: codes.InvalidArgument},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			srv := New("t", nil, errClient{err: tc.err}, nil)
			_, err := srv.List(context.Background(), &axiomv1.ListRequest{Gvk: podGVK})
			if got := status.Code(err); got != tc.want {
				t.Fatalf("code = %v, want %v (err=%v)", got, tc.want, err)
			}
			_, err = srv.Get(context.Background(), &axiomv1.GetRequest{Gvk: podGVK, Namespace: "d", Name: "a"})
			if got := status.Code(err); got != tc.want {
				t.Fatalf("get code = %v, want %v (err=%v)", got, tc.want, err)
			}
			_, err = srv.Create(context.Background(), &axiomv1.CreateRequest{Gvk: cmGVK, Namespace: "d", Name: "a", Json: []byte(`{}`)})
			if got := status.Code(err); got != tc.want {
				t.Fatalf("create code = %v, want %v (err=%v)", got, tc.want, err)
			}
			_, err = srv.Update(context.Background(), &axiomv1.UpdateRequest{Gvk: cmGVK, Namespace: "d", Name: "a", ResourceVersion: "1", Json: []byte(`{}`)})
			if got := status.Code(err); got != tc.want {
				t.Fatalf("update code = %v, want %v (err=%v)", got, tc.want, err)
			}
			_, err = srv.Delete(context.Background(), &axiomv1.DeleteRequest{Gvk: cmGVK, Namespace: "d", Name: "a"})
			if got := status.Code(err); got != tc.want {
				t.Fatalf("delete code = %v, want %v (err=%v)", got, tc.want, err)
			}
		})
	}
}

// pagingClient is a Client that actually honours Limit and Continue.
//
// client-go's dynamicfake ignores both, so every existing test exercises the
// unpaged path no matter what the server asks for. Without a fake that pages,
// a regression in forwarding the continuation, shrinking an oversized page, or
// reporting a single object over the budget would pass unnoticed.
type pagingClient struct {
	k8s.Client
	items []unstructured.Unstructured
	// limits records the limit of each call, so a test can assert the server
	// actually halved rather than merely returned something acceptable.
	limits []int64
}

func (p *pagingClient) List(_ context.Context, _ schema.GroupVersionKind, _, _ string, limit int64, cont string) (*unstructured.UnstructuredList, error) {
	p.limits = append(p.limits, limit)
	start := 0
	if cont != "" {
		n, err := strconv.Atoi(cont)
		if err != nil {
			return nil, fmt.Errorf("bad continue token %q", cont)
		}
		start = n
	}
	if start > len(p.items) {
		start = len(p.items)
	}
	end := len(p.items)
	if limit > 0 && start+int(limit) < end {
		end = start + int(limit)
	}
	out := &unstructured.UnstructuredList{Items: append([]unstructured.Unstructured(nil), p.items[start:end]...)}
	if end < len(p.items) {
		out.SetContinue(strconv.Itoa(end))
	}
	return out, nil
}

// padded builds a ConfigMap whose JSON is at least n bytes.
func padded(name string, n int) unstructured.Unstructured {
	return unstructured.Unstructured{Object: map[string]any{
		"apiVersion": "v1",
		"kind":       "ConfigMap",
		"metadata":   map[string]any{"name": name, "namespace": "default"},
		"data":       map[string]any{"blob": strings.Repeat("x", n)},
	}}
}

func TestListForwardsTheContinuationToken(t *testing.T) {
	t.Parallel()
	items := make([]unstructured.Unstructured, 7)
	for i := range items {
		items[i] = padded(fmt.Sprintf("cm-%d", i), 8)
	}
	pc := &pagingClient{items: items}
	s := New("test", nil, pc, nil)
	gvk := &axiomv1.GroupVersionKind{Version: "v1", Kind: "ConfigMap"}

	// First page, limited to 3.
	r1, err := s.List(context.Background(), &axiomv1.ListRequest{Gvk: gvk, Limit: 3})
	if err != nil {
		t.Fatal(err)
	}
	if len(r1.GetObjects()) != 3 || r1.GetContinueToken() == "" {
		t.Fatalf("first page: %d objects, token %q; want 3 and a token",
			len(r1.GetObjects()), r1.GetContinueToken())
	}

	// Resuming walks forward rather than starting again.
	r2, err := s.List(context.Background(), &axiomv1.ListRequest{
		Gvk: gvk, Limit: 3, ContinueToken: r1.GetContinueToken(),
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(r2.GetObjects()) != 3 {
		t.Fatalf("second page: %d objects, want 3", len(r2.GetObjects()))
	}
	if string(r2.GetObjects()[0].GetJson()) == string(r1.GetObjects()[0].GetJson()) {
		t.Error("resuming returned the first page again: the token was not forwarded")
	}

	// Last page is short and ends the walk.
	r3, err := s.List(context.Background(), &axiomv1.ListRequest{
		Gvk: gvk, Limit: 3, ContinueToken: r2.GetContinueToken(),
	})
	if err != nil {
		t.Fatal(err)
	}
	if len(r3.GetObjects()) != 1 || r3.GetContinueToken() != "" {
		t.Fatalf("last page: %d objects, token %q; want 1 and no token",
			len(r3.GetObjects()), r3.GetContinueToken())
	}
}

func TestListShrinksAPageThatExceedsTheByteBudget(t *testing.T) {
	t.Parallel()
	// Eight objects of 1 MiB: four of them already exceed the 4 MiB budget,
	// so the server must ask for fewer.
	items := make([]unstructured.Unstructured, 8)
	for i := range items {
		items[i] = padded(fmt.Sprintf("big-%d", i), 1<<20)
	}
	pc := &pagingClient{items: items}
	s := New("test", nil, pc, nil)

	resp, err := s.List(context.Background(), &axiomv1.ListRequest{
		Gvk:   &axiomv1.GroupVersionKind{Version: "v1", Kind: "ConfigMap"},
		Limit: 8,
	})
	if err != nil {
		t.Fatal(err)
	}
	total := 0
	for _, o := range resp.GetObjects() {
		total += len(o.GetJson())
	}
	if total > maxPageBytes {
		t.Errorf("returned %d bytes, over the %d budget", total, maxPageBytes)
	}
	if len(pc.limits) < 2 {
		t.Fatalf("expected the server to retry with a smaller limit, limits were %v", pc.limits)
	}
	if pc.limits[1] >= pc.limits[0] {
		t.Errorf("limit did not shrink: %v", pc.limits)
	}
	// A short page must still be resumable, or the rest is silently lost.
	if resp.GetContinueToken() == "" {
		t.Error("a shrunk page must carry a continue token")
	}
}

func TestListReportsASingleObjectOverTheBudget(t *testing.T) {
	t.Parallel()
	pc := &pagingClient{items: []unstructured.Unstructured{padded("huge", 5<<20)}}
	s := New("test", nil, pc, nil)

	_, err := s.List(context.Background(), &axiomv1.ListRequest{
		Gvk: &axiomv1.GroupVersionKind{Version: "v1", Kind: "ConfigMap"}, Limit: 1,
	})
	if status.Code(err) != codes.ResourceExhausted {
		t.Fatalf("err = %v (code %v), want ResourceExhausted", err, status.Code(err))
	}
	// Paging cannot split one object, so the message must say so rather than
	// leaving the caller to retry something that can never succeed.
	if !strings.Contains(err.Error(), "cannot be split further") {
		t.Errorf("error should explain that paging cannot help: %v", err)
	}
}
