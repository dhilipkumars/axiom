package k8s

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"testing"
	"time"

	corev1 "k8s.io/api/core/v1"
	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/apis/meta/v1/unstructured"
	"k8s.io/apimachinery/pkg/runtime"
	"k8s.io/apimachinery/pkg/runtime/schema"
	dynamicfake "k8s.io/client-go/dynamic/fake"
	"k8s.io/client-go/kubernetes/scheme"
	k8stesting "k8s.io/client-go/testing"
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
		pod("default", "a"), pod("default", "b"), pod("other", "c")),
		NewStaticMapper(BuiltinKinds()...))
}

func TestResolve(t *testing.T) {
	t.Parallel()
	m := NewStaticMapper(BuiltinKinds()...)
	gvr, namespaced, err := m.Resolve(context.Background(), podGVK)
	if err != nil || gvr.Resource != "pods" || !namespaced {
		t.Fatalf("Resolve(Pod) = %v %v %v", gvr, namespaced, err)
	}
	_, _, err = m.Resolve(context.Background(), schema.GroupVersionKind{Group: "apps", Version: "v1", Kind: "Deployment"})
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

func cm(ns, name string, data map[string]string) *unstructured.Unstructured {
	u := &unstructured.Unstructured{}
	u.SetAPIVersion("v1")
	u.SetKind("ConfigMap")
	u.SetNamespace(ns)
	u.SetName(name)
	if data != nil {
		m := map[string]any{}
		for k, v := range data {
			m[k] = v
		}
		_ = unstructured.SetNestedField(u.Object, m, "data") // static keys, cannot fail
	}
	return u
}

var cmGVK = schema.GroupVersionKind{Version: "v1", Kind: "ConfigMap"}

func TestDynamicWrites(t *testing.T) {
	t.Parallel()
	c := newFake(t)
	ctx := context.Background()

	created, err := c.Create(ctx, cmGVK, "default", cm("default", "app", map[string]string{"k": "v"}))
	if err != nil || created.GetName() != "app" {
		t.Fatalf("Create = %v, %v", created, err)
	}
	if _, err := c.Create(ctx, cmGVK, "default", cm("default", "app", nil)); !apierrors.IsAlreadyExists(err) {
		t.Fatalf("duplicate Create err = %v, want AlreadyExists", err)
	}

	created.Object["data"] = map[string]any{"k": "v2"}
	updated, err := c.Update(ctx, cmGVK, "default", created)
	if err != nil {
		t.Fatal(err)
	}
	if got, _, _ := unstructured.NestedString(updated.Object, "data", "k"); got != "v2" {
		t.Fatalf("data.k after update = %q", got)
	}
	if _, err := c.Update(ctx, cmGVK, "default", cm("default", "ghost", nil)); !apierrors.IsNotFound(err) {
		t.Fatalf("Update missing err = %v, want NotFound", err)
	}

	if err := c.Delete(ctx, cmGVK, "default", "app"); err != nil {
		t.Fatal(err)
	}
	if err := c.Delete(ctx, cmGVK, "default", "app"); !apierrors.IsNotFound(err) {
		t.Fatalf("second Delete err = %v, want NotFound", err)
	}
	if _, err := c.Create(ctx, schema.GroupVersionKind{Kind: "Nope", Version: "v1"}, "default", cm("default", "x", nil)); !errors.Is(err, ErrUnsupportedKind) {
		t.Fatalf("unsupported kind err = %v", err)
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
	if _, err := c.Create(context.Background(), cmGVK, "d", cm("d", "a", nil)); !errors.Is(err, ErrNoCluster) {
		t.Fatalf("Create err = %v", err)
	}
	if _, err := c.Update(context.Background(), cmGVK, "d", cm("d", "a", nil)); !errors.Is(err, ErrNoCluster) {
		t.Fatalf("Update err = %v", err)
	}
	if err := c.Delete(context.Background(), cmGVK, "d", "a"); !errors.Is(err, ErrNoCluster) {
		t.Fatalf("Delete err = %v", err)
	}
	if _, err := c.Watch(context.Background(), podGVK, "", ""); !errors.Is(err, ErrNoCluster) {
		t.Fatalf("Watch err = %v", err)
	}
}

func TestDynamicWatch(t *testing.T) {
	t.Parallel()
	c := newFake(t)
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	w, err := c.Watch(ctx, podGVK, "default", "")
	if err != nil {
		t.Fatal(err)
	}
	defer w.Stop()
	if _, err := c.Create(ctx, podGVK, "default", func() *unstructured.Unstructured {
		u := &unstructured.Unstructured{}
		u.SetAPIVersion("v1")
		u.SetKind("Pod")
		u.SetNamespace("default")
		u.SetName("new-pod")
		return u
	}()); err != nil {
		t.Fatal(err)
	}
	select {
	case ev := <-w.ResultChan():
		if ev.Type != "ADDED" {
			t.Fatalf("event type = %s, want ADDED", ev.Type)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("no watch event")
	}
	if _, err := c.Watch(ctx, schema.GroupVersionKind{Kind: "Nope", Version: "v1"}, "", ""); !errors.Is(err, ErrUnsupportedKind) {
		t.Fatalf("unsupported kind err = %v", err)
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

// TestListByNameAcrossAllNamespaces covers a lookup that has no namespace.
//
// Serving a whole cluster makes "find this object wherever it lives" an
// ordinary query. A point Get cannot express it for a namespaced kind: the
// request would omit the namespace segment and 404, which the List path used
// to swallow into an empty result, so `WHERE name = 'x'` silently returned
// nothing unless a namespace was also given.
//
// The assertion is on the request rather than the result because client-go's
// fake dynamic client does not honour field selectors -- its tracker returns
// the whole collection regardless. Filtering in the gateway instead would
// contradict the contract in axiom.proto that List narrows server-side and
// never fetches everything to discard most of it, so what is verifiable here
// is that the selector is sent. e2e/cluster_test.sh covers the real behaviour
// against an API server that honours it.
func TestListByNameAcrossAllNamespaces(t *testing.T) {
	t.Parallel()
	dyn := dynamicfake.NewSimpleDynamicClient(scheme.Scheme,
		pod("default", "a"), pod("other", "c"))
	var gotSelector string
	var listed int
	dyn.PrependReactor("list", "pods", func(action k8stesting.Action) (bool, runtime.Object, error) {
		listed++
		gotSelector = action.(k8stesting.ListAction).GetListRestrictions().Fields.String()
		return false, nil, nil
	})
	c := NewDynamic(dyn, NewStaticMapper(BuiltinKinds()...))

	if _, err := c.List(context.Background(), podGVK, "", "c"); err != nil {
		t.Fatalf("List(name=c, no namespace) = %v", err)
	}
	if listed != 1 {
		t.Fatalf("issued %d LISTs, want 1: a name-only lookup must not fall back to a point Get", listed)
	}
	if gotSelector != "metadata.name=c" {
		t.Errorf("field selector = %q, want metadata.name=c; without it the gateway would "+
			"fetch every namespace's pods and discard most of them", gotSelector)
	}
}

// TestListByNameWithNamespaceStillUsesAPointGet pins the Phase 1 optimisation:
// when the object can be named in full, a Get is cheaper for the API server
// than a field-selector LIST over the collection.
func TestListByNameWithNamespaceStillUsesAPointGet(t *testing.T) {
	t.Parallel()
	dyn := dynamicfake.NewSimpleDynamicClient(scheme.Scheme, pod("default", "a"))
	var lists, gets int
	dyn.PrependReactor("list", "pods", func(k8stesting.Action) (bool, runtime.Object, error) {
		lists++
		return false, nil, nil
	})
	dyn.PrependReactor("get", "pods", func(k8stesting.Action) (bool, runtime.Object, error) {
		gets++
		return false, nil, nil
	})
	c := NewDynamic(dyn, NewStaticMapper(BuiltinKinds()...))

	got, err := c.List(context.Background(), podGVK, "default", "a")
	if err != nil {
		t.Fatal(err)
	}
	if len(got.Items) != 1 || got.Items[0].GetName() != "a" {
		t.Fatalf("List(ns=default, name=a) = %v, want the one pod", got.Items)
	}
	if gets != 1 || lists != 0 {
		t.Errorf("gets=%d lists=%d, want a single point Get", gets, lists)
	}

	// A miss is an empty result, not an error.
	got, err = c.List(context.Background(), podGVK, "default", "nonexistent")
	if err != nil || len(got.Items) != 0 {
		t.Errorf("missing object: got %v, %v; want an empty result and no error", got, err)
	}
}
