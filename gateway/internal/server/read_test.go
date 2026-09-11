package server

import (
	"context"
	"encoding/json"
	"errors"
	"net/http"
	"testing"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	corev1 "k8s.io/api/core/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/apis/meta/v1/unstructured"
	"k8s.io/apimachinery/pkg/runtime/schema"
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
	return New("t", nil, k8s.NewDynamic(dyn), nil)
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
		{name: "namespace+name", req: &axiomv1.ListRequest{Gvk: podGVK, Namespace: "default", Name: "db"}, wantNames: map[string]bool{"db": true}},
		{name: "name miss is empty", req: &axiomv1.ListRequest{Gvk: podGVK, Namespace: "default", Name: "zzz"}, wantNames: map[string]bool{}},
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

func (e errClient) List(context.Context, schema.GroupVersionKind, string, string) (*unstructured.UnstructuredList, error) {
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
