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

	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/apis/meta/v1/unstructured"
	"k8s.io/apimachinery/pkg/fields"
	"k8s.io/apimachinery/pkg/runtime/schema"
	"k8s.io/apimachinery/pkg/watch"
	"k8s.io/client-go/dynamic"
	"k8s.io/client-go/rest"
	"k8s.io/client-go/tools/clientcmd"
)

// Client is the gateway's cluster surface: reads (Phase 1), writes (Phase 2),
// watch (Phase 3) and schema discovery (Phase 4). Implementations must return
// apimachinery StatusErrors (apierrors.IsNotFound, IsConflict, ...) so callers
// can map them to gRPC codes.
//
// Discovery is part of this interface rather than a second dependency so that
// "what this gateway can serve" and "what it does with a served kind" can never
// disagree: the same Mapper that resolves a scan's GVK is the one that decided
// the kind was servable in the first place.
type Client interface {
	Mapper

	// Get returns one object. Namespace must be empty for cluster-scoped kinds.
	Get(ctx context.Context, gvk schema.GroupVersionKind, namespace, name string) (*unstructured.Unstructured, error)
	// List returns objects of one kind. Empty namespace means all namespaces.
	//
	// A non-empty name narrows to objects of that name, applied server-side as
	// a metadata.name field selector. It is never served by a point Get: `get`
	// and `list` are independent RBAC verbs, and a kind is admitted on `list`,
	// so using `get` anywhere on the read path would make a list-only kind
	// importable but unqueryable once a name filter appeared.
	//
	// A name filter yields at most one object when paired with a namespace, or
	// for a cluster-scoped kind. Across all namespaces it can yield several,
	// since a name is unique only within one. A filter matching nothing is an
	// empty list, not an error.
	// List returns one page. limit bounds the objects returned (zero lets the
	// API server choose) and continueToken resumes a previous page. The
	// returned list's GetContinue() is non-empty when more remain.
	List(ctx context.Context, gvk schema.GroupVersionKind, namespace, name string, limit int64, continueToken string) (*unstructured.UnstructuredList, error)
	// Create creates obj. apiVersion/kind must already match gvk.
	Create(ctx context.Context, gvk schema.GroupVersionKind, namespace string, obj *unstructured.Unstructured) (*unstructured.Unstructured, error)
	// Update replaces obj (PUT). obj must carry metadata.resourceVersion; the
	// API server returns a Conflict StatusError if it is stale.
	Update(ctx context.Context, gvk schema.GroupVersionKind, namespace string, obj *unstructured.Unstructured) (*unstructured.Unstructured, error)
	// Delete deletes by name.
	Delete(ctx context.Context, gvk schema.GroupVersionKind, namespace, name string) error
	// Watch opens a watch from resourceVersion (empty = from now) with
	// bookmarks enabled. The returned watcher's channel closes when the API
	// server ends the watch; callers re-watch from the last seen version.
	Watch(ctx context.Context, gvk schema.GroupVersionKind, namespace, resourceVersion string) (watch.Interface, error)
}

// StatsReporter exposes discovery and authorization statistics gathered by the
// underlying client components.
type StatsReporter interface {
	// OpenAPIFetches reports the total number of OpenAPI v3 document fetches performed.
	OpenAPIFetches() uint64
	// OpenAPIGroupVersions reports the number of distinct group-versions currently cached.
	OpenAPIGroupVersions() uint64
	// AccessReviews reports the total number of SelfSubjectAccessReview calls issued.
	AccessReviews() uint64
}

// ErrUnsupportedKind is returned for a GVK the gateway does not serve.
var ErrUnsupportedKind = errors.New("unsupported kind")

// ErrNoCluster is returned by Unconfigured for every read.
var ErrNoCluster = errors.New("gateway has no cluster credentials configured")

// Dynamic is a Client over client-go's dynamic interface, resolving kinds
// through a Mapper rather than a compile-time table.
type Dynamic struct {
	dyn dynamic.Interface
	Mapper
}

// OpenAPIFetches implements StatsReporter by delegating to Mapper if supported.
func (c *Dynamic) OpenAPIFetches() uint64 {
	if sr, ok := c.Mapper.(StatsReporter); ok {
		return sr.OpenAPIFetches()
	}
	return 0
}

// OpenAPIGroupVersions implements StatsReporter by delegating to Mapper if supported.
func (c *Dynamic) OpenAPIGroupVersions() uint64 {
	if sr, ok := c.Mapper.(StatsReporter); ok {
		return sr.OpenAPIGroupVersions()
	}
	return 0
}

// AccessReviews implements StatsReporter by delegating to Mapper if supported.
func (c *Dynamic) AccessReviews() uint64 {
	if sr, ok := c.Mapper.(StatsReporter); ok {
		return sr.AccessReviews()
	}
	return 0
}

// NewDynamic wraps an existing dynamic client (real or fake) and the Mapper
// that decides which kinds it serves. A nil Mapper serves nothing, so a
// miswired gateway fails loudly on the first scan instead of resolving
// everything (docs/RULES.md §1).
func NewDynamic(d dynamic.Interface, m Mapper) *Dynamic {
	if m == nil {
		m = NewStaticMapper()
	}
	return &Dynamic{dyn: d, Mapper: m}
}

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

// NewFromConfig builds a Dynamic client from REST config, resolving kinds
// through m.
func NewFromConfig(cfg *rest.Config, m Mapper) (*Dynamic, error) {
	d, err := dynamic.NewForConfig(cfg)
	if err != nil {
		return nil, fmt.Errorf("k8s: dynamic client: %w", err)
	}
	return NewDynamic(d, m), nil
}

func (c *Dynamic) resourceFor(ctx context.Context, gvk schema.GroupVersionKind, namespace string) (dynamic.ResourceInterface, error) {
	gvr, namespaced, err := c.Resolve(ctx, gvk)
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
	ri, err := c.resourceFor(ctx, gvk, namespace)
	if err != nil {
		return nil, err
	}
	return ri.Get(ctx, name, metav1.GetOptions{})
}

// List implements Client.
func (c *Dynamic) List(ctx context.Context, gvk schema.GroupVersionKind, namespace, name string, limit int64, continueToken string) (*unstructured.UnstructuredList, error) {
	ri, err := c.resourceFor(ctx, gvk, namespace)
	if err != nil {
		return nil, err
	}
	opts := metav1.ListOptions{Limit: limit, Continue: continueToken}
	if name != "" {
		// Narrow server-side. A point Get would be marginally cheaper when the
		// object can be named in full, and Phase 1 used one for exactly that
		// reason, but `get` and `list` are independent RBAC verbs and Phase 5
		// admits a kind on `list` alone. Serving any part of the read path with
		// `get` would let a list-only kind import cleanly and then fail with
		// PERMISSION_DENIED the moment a query added a name filter. One verb
		// for the whole read path keeps what is offered and what works the
		// same thing. metadata.name is an indexed field selector, so the API
		// server does not scan the collection to answer this.
		opts.FieldSelector = fields.OneTermEqualSelector("metadata.name", name).String()
	}
	return ri.List(ctx, opts)
}

// Create implements Client.
func (c *Dynamic) Create(ctx context.Context, gvk schema.GroupVersionKind, namespace string, obj *unstructured.Unstructured) (*unstructured.Unstructured, error) {
	ri, err := c.resourceFor(ctx, gvk, namespace)
	if err != nil {
		return nil, err
	}
	return ri.Create(ctx, obj, metav1.CreateOptions{FieldManager: fieldManager})
}

// Update implements Client.
func (c *Dynamic) Update(ctx context.Context, gvk schema.GroupVersionKind, namespace string, obj *unstructured.Unstructured) (*unstructured.Unstructured, error) {
	ri, err := c.resourceFor(ctx, gvk, namespace)
	if err != nil {
		return nil, err
	}
	return ri.Update(ctx, obj, metav1.UpdateOptions{FieldManager: fieldManager})
}

// Delete implements Client.
func (c *Dynamic) Delete(ctx context.Context, gvk schema.GroupVersionKind, namespace, name string) error {
	ri, err := c.resourceFor(ctx, gvk, namespace)
	if err != nil {
		return err
	}
	return ri.Delete(ctx, name, metav1.DeleteOptions{})
}

// Watch implements Client.
func (c *Dynamic) Watch(ctx context.Context, gvk schema.GroupVersionKind, namespace, resourceVersion string) (watch.Interface, error) {
	ri, err := c.resourceFor(ctx, gvk, namespace)
	if err != nil {
		return nil, err
	}
	return ri.Watch(ctx, metav1.ListOptions{ResourceVersion: resourceVersion, AllowWatchBookmarks: true})
}

// fieldManager identifies Axiom's writes in managedFields so operators can
// see which fields SQL last set.
const fieldManager = "axiom"

// Unconfigured is the Client used when the gateway was started without any
// cluster credentials. Every read fails with ErrNoCluster so the condition is
// loud rather than an empty result (docs/RULES.md §1, no silent degrade).
type Unconfigured struct{}

// Get implements Client.
func (Unconfigured) Get(context.Context, schema.GroupVersionKind, string, string) (*unstructured.Unstructured, error) {
	return nil, ErrNoCluster
}

// List implements Client.
func (Unconfigured) List(context.Context, schema.GroupVersionKind, string, string, int64, string) (*unstructured.UnstructuredList, error) {
	return nil, ErrNoCluster
}

// Create implements Client.
func (Unconfigured) Create(context.Context, schema.GroupVersionKind, string, *unstructured.Unstructured) (*unstructured.Unstructured, error) {
	return nil, ErrNoCluster
}

// Update implements Client.
func (Unconfigured) Update(context.Context, schema.GroupVersionKind, string, *unstructured.Unstructured) (*unstructured.Unstructured, error) {
	return nil, ErrNoCluster
}

// Delete implements Client.
func (Unconfigured) Delete(context.Context, schema.GroupVersionKind, string, string) error {
	return ErrNoCluster
}

// Watch implements Client.
func (Unconfigured) Watch(context.Context, schema.GroupVersionKind, string, string) (watch.Interface, error) {
	return nil, ErrNoCluster
}

// Resolve implements Mapper.
func (Unconfigured) Resolve(context.Context, schema.GroupVersionKind) (schema.GroupVersionResource, bool, error) {
	return schema.GroupVersionResource{}, false, ErrNoCluster
}

// Describe implements Mapper.
func (Unconfigured) Describe(context.Context, schema.GroupVersionKind) (KindInfo, error) {
	return KindInfo{}, ErrNoCluster
}

// Kinds implements Mapper. It reports the error rather than an empty list: an
// empty enumeration would make IMPORT FOREIGN SCHEMA succeed with no tables,
// which reads as "this cluster has nothing" instead of "this gateway has no
// cluster" (docs/RULES.md §1).
func (Unconfigured) Kinds(context.Context, *string, []string) ([]KindInfo, error) {
	return nil, ErrNoCluster
}
