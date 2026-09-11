package k8s

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"testing"

	corev1 "k8s.io/api/core/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime/schema"
	dynamicfake "k8s.io/client-go/dynamic/fake"
	"k8s.io/client-go/kubernetes/scheme"
)

var podGVK = schema.GroupVersionKind{Version: "v1", Kind: "Pod"}

func pod(ns, name string) *corev1.Pod {
	return &corev1.Pod{
		TypeMeta:   metav1.TypeMeta{APIVersion: "v1", Kind: "Pod"},
		ObjectMeta: metav1.ObjectMeta{Namespace: ns, Name: name},
	}
}

func newFake(t *testing.T) *Dynamic {
	t.Helper()
	return NewDynamic(dynamicfake.NewSimpleDynamicClient(scheme.Scheme,
		pod("default", "a"), pod("default", "b"), pod("other", "c")))
}

func TestResolve(t *testing.T) {
	t.Parallel()
	gvr, namespaced, err := Resolve(podGVK)
	if err != nil || gvr.Resource != "pods" || !namespaced {
		t.Fatalf("Resolve(Pod) = %v %v %v", gvr, namespaced, err)
	}
	_, _, err = Resolve(schema.GroupVersionKind{Group: "apps", Version: "v1", Kind: "Deployment"})
	if !errors.Is(err, ErrUnsupportedKind) {
		t.Fatalf("err = %v, want ErrUnsupportedKind", err)
	}
}

func TestDynamicGet(t *testing.T) {
	t.Parallel()
	c := newFake(t)
	obj, err := c.Get(context.Background(), podGVK, "default", "a")
	if err != nil || obj.GetName() != "a" {
		t.Fatalf("Get = %v, %v", obj, err)
	}
	_, err = c.Get(context.Background(), podGVK, "default", "missing")
	if !apierrors.IsNotFound(err) {
		t.Fatalf("err = %v, want NotFound", err)
	}
	_, err = c.Get(context.Background(), schema.GroupVersionKind{Kind: "Nope", Version: "v1"}, "default", "a")
	if !errors.Is(err, ErrUnsupportedKind) {
		t.Fatalf("err = %v, want ErrUnsupportedKind", err)
	}
}

func TestDynamicList(t *testing.T) {
	t.Parallel()
	c := newFake(t)
	tests := []struct {
		name      string
		ns, obj   string
		wantNames []string
	}{
		{name: "all namespaces", wantNames: []string{"a", "b", "c"}},
		{name: "one namespace", ns: "default", wantNames: []string{"a", "b"}},
		{name: "namespace + name", ns: "default", obj: "b", wantNames: []string{"b"}},
		{name: "name miss is empty not error", ns: "default", obj: "zzz", wantNames: nil},
		{name: "empty namespace", ns: "empty", wantNames: nil},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			list, err := c.List(context.Background(), podGVK, tc.ns, tc.obj)
			if err != nil {
				t.Fatal(err)
			}
			var got []string
			for _, it := range list.Items {
				got = append(got, it.GetName())
			}
			if len(got) != len(tc.wantNames) {
				t.Fatalf("names = %v, want %v", got, tc.wantNames)
			}
			for i := range got {
				if got[i] != tc.wantNames[i] {
					t.Fatalf("names = %v, want %v", got, tc.wantNames)
				}
			}
		})
	}
}

func TestUnconfigured(t *testing.T) {
	t.Parallel()
	var c Client = Unconfigured{}
	if _, err := c.Get(context.Background(), podGVK, "d", "a"); !errors.Is(err, ErrNoCluster) {
		t.Fatalf("Get err = %v", err)
	}
	if _, err := c.List(context.Background(), podGVK, "", ""); !errors.Is(err, ErrNoCluster) {
		t.Fatalf("List err = %v", err)
	}
}

func TestConfigErrors(t *testing.T) {
	// Not parallel: uses t.Setenv.
	if _, err := Config(filepath.Join(t.TempDir(), "missing")); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("missing kubeconfig err = %v, want ErrNotExist", err)
	}
	bad := filepath.Join(t.TempDir(), "kubeconfig")
	if err := os.WriteFile(bad, []byte("not: [valid"), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, err := Config(bad); err == nil {
		t.Fatal("expected error for malformed kubeconfig")
	}
	// No kubeconfig and not in a cluster: must fail, never silently succeed.
	t.Setenv("KUBERNETES_SERVICE_HOST", "")
	if _, err := Config(""); err == nil {
		t.Fatal("expected in-cluster config error outside a cluster")
	}
}
