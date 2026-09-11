// Package k8s is the gateway's only contact with the Kubernetes API.
//
// Everything cluster-facing sits behind the narrow Client interface so RPC
// handlers are unit-tested against client-go's fake dynamic client with no
// real cluster (docs/RULES.md §2). Credentials (kubeconfig, SA token) are
// loaded here and never leave this process or appear in errors (§3).
package k8s

import (
	"context"
	"errors"
	"fmt"
	"os"

	apierrors "k8s.io/apimachinery/pkg/api/errors"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/apis/meta/v1/unstructured"
	"k8s.io/apimachinery/pkg/runtime/schema"
	"k8s.io/client-go/dynamic"
	"k8s.io/client-go/rest"
	"k8s.io/client-go/tools/clientcmd"
)

// Client is the read surface the gateway needs in Phase 1. Implementations
// must return apimachinery StatusErrors (apierrors.IsNotFound etc.) so callers
// can map them to gRPC codes.
type Client interface {
	// Get returns one object. Namespace must be empty for cluster-scoped kinds.
	Get(ctx context.Context, gvk schema.GroupVersionKind, namespace, name string) (*unstructured.Unstructured, error)
	// List returns objects of one kind. Empty namespace means all namespaces.
	// If name is non-empty the result holds at most that one object; this is
	// served with a point Get (cheaper for the API server than a filtered
	// LIST, which scans the whole collection server-side) and a miss yields an
	// empty list, not an error.
	List(ctx context.Context, gvk schema.GroupVersionKind, namespace, name string) (*unstructured.UnstructuredList, error)
}

// ErrUnsupportedKind is returned for a GVK the gateway does not serve.
var ErrUnsupportedKind = errors.New("unsupported kind")

// ErrNoCluster is returned by Unconfigured for every read.
var ErrNoCluster = errors.New("gateway has no cluster credentials configured")

// resource describes how a served kind maps onto the REST API.
type resource struct {
	gvr        schema.GroupVersionResource
	namespaced bool
}

// registry is the static set of kinds served. Phase 1 serves Pods only.
// TODO(phase4): replace with RESTMapper-backed discovery so CRDs resolve.
var registry = map[schema.GroupVersionKind]resource{
	{Group: "", Version: "v1", Kind: "Pod"}: {
		gvr:        schema.GroupVersionResource{Group: "", Version: "v1", Resource: "pods"},
		namespaced: true,
	},
}

// Resolve maps a GVK to its REST resource. Returns ErrUnsupportedKind (wrapped
// with the GVK) for anything outside the registry.
func Resolve(gvk schema.GroupVersionKind) (schema.GroupVersionResource, bool, error) {
	r, ok := registry[gvk]
	if !ok {
		return schema.GroupVersionResource{}, false, fmt.Errorf("%w: %s", ErrUnsupportedKind, gvk.String())
	}
	return r.gvr, r.namespaced, nil
}

// Dynamic is a Client over client-go's dynamic interface.
type Dynamic struct {
	dyn dynamic.Interface
}

// NewDynamic wraps an existing dynamic client (real or fake).
func NewDynamic(d dynamic.Interface) *Dynamic { return &Dynamic{dyn: d} }

// Config loads REST config from kubeconfigPath, or in-cluster config when the
// path is empty. Errors wrap the underlying cause but never include token or
// certificate material (client-go's loader errors describe files, not contents).
func Config(kubeconfigPath string) (*rest.Config, error) {
	if kubeconfigPath == "" {
		cfg, err := rest.InClusterConfig()
		if err != nil {
			return nil, fmt.Errorf("k8s: in-cluster config: %w", err)
		}
		return cfg, nil
	}
	if _, err := os.Stat(kubeconfigPath); err != nil {
		return nil, fmt.Errorf("k8s: kubeconfig %q: %w", kubeconfigPath, err)
	}
	cfg, err := clientcmd.BuildConfigFromFlags("", kubeconfigPath)
	if err != nil {
		return nil, fmt.Errorf("k8s: load kubeconfig %q: %w", kubeconfigPath, err)
	}
	return cfg, nil
}

// NewFromConfig builds a Dynamic client from REST config.
func NewFromConfig(cfg *rest.Config) (*Dynamic, error) {
	d, err := dynamic.NewForConfig(cfg)
	if err != nil {
		return nil, fmt.Errorf("k8s: dynamic client: %w", err)
	}
	return NewDynamic(d), nil
}

func (c *Dynamic) resourceFor(gvk schema.GroupVersionKind, namespace string) (dynamic.ResourceInterface, error) {
	gvr, namespaced, err := Resolve(gvk)
	if err != nil {
		return nil, err
	}
	if !namespaced && namespace != "" {
		return nil, fmt.Errorf("%w: %s is cluster-scoped but namespace %q was given", ErrUnsupportedKind, gvk.Kind, namespace)
	}
	if namespace == "" {
		return c.dyn.Resource(gvr), nil
	}
	return c.dyn.Resource(gvr).Namespace(namespace), nil
}

// Get implements Client.
func (c *Dynamic) Get(ctx context.Context, gvk schema.GroupVersionKind, namespace, name string) (*unstructured.Unstructured, error) {
	ri, err := c.resourceFor(gvk, namespace)
	if err != nil {
		return nil, err
	}
	return ri.Get(ctx, name, metav1.GetOptions{})
}

// List implements Client.
func (c *Dynamic) List(ctx context.Context, gvk schema.GroupVersionKind, namespace, name string) (*unstructured.UnstructuredList, error) {
	if name != "" {
		obj, err := c.Get(ctx, gvk, namespace, name)
		if apierrors.IsNotFound(err) {
			return &unstructured.UnstructuredList{}, nil
		}
		if err != nil {
			return nil, err
		}
		return &unstructured.UnstructuredList{Items: []unstructured.Unstructured{*obj}}, nil
	}
	ri, err := c.resourceFor(gvk, namespace)
	if err != nil {
		return nil, err
	}
	return ri.List(ctx, metav1.ListOptions{})
}

// Unconfigured is the Client used when the gateway was started without any
// cluster credentials. Every read fails with ErrNoCluster so the condition is
// loud rather than an empty result (docs/RULES.md §1, no silent degrade).
type Unconfigured struct{}

// Get implements Client.
func (Unconfigured) Get(context.Context, schema.GroupVersionKind, string, string) (*unstructured.Unstructured, error) {
	return nil, ErrNoCluster
}

// List implements Client.
func (Unconfigured) List(context.Context, schema.GroupVersionKind, string, string) (*unstructured.UnstructuredList, error) {
	return nil, ErrNoCluster
}
