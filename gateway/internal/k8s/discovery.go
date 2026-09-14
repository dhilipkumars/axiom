package k8s

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"log/slog"
	"sort"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime/schema"
	"k8s.io/client-go/discovery"
	"k8s.io/client-go/openapi"
)

// Mapper resolves Kubernetes kinds against a cluster's discovery documents.
//
// It is the seam that replaced the Phase 1-3 static registry: everything that
// used to be a compile-time map of two kinds is now a runtime lookup, bounded
// by an Allowlist. It is an interface so the RPC handlers unit-test against a
// fake with no cluster and no discovery round-trips (docs/RULES.md §2).
type Mapper interface {
	// Resolve maps a GVK to its REST resource and scope.
	//
	// Returns ErrUnsupportedKind if the cluster has no such kind or the
	// gateway's allowlist excludes it. The two are deliberately the same
	// error: which kinds a gateway serves is not information a caller is
	// entitled to enumerate by probing.
	Resolve(ctx context.Context, gvk schema.GroupVersionKind) (schema.GroupVersionResource, bool, error)

	// Describe returns the full discovered shape of one kind, including the
	// columns its foreign table should have.
	//
	// Returns ErrUnsupportedKind as Resolve does, and an error if the kind's
	// schema cannot be read. It never returns a KindInfo with a partial
	// column list: a thin table would look like a kind with few fields rather
	// than like a failed lookup (docs/RULES.md §1).
	Describe(ctx context.Context, gvk schema.GroupVersionKind) (KindInfo, error)

	// Kinds enumerates the kinds this gateway serves, narrowed to one API
	// group when group is non-nil and to an explicit set of plural names when
	// plurals is non-empty. The result is sorted by group then plural so
	// generated DDL is stable across calls.
	Kinds(ctx context.Context, group *string, plurals []string) ([]KindInfo, error)
}

// resourceCacheEntry is one group-version's resolved resources.
type resourceCacheEntry struct {
	// byKind maps a Kind to its APIResource, subresources excluded.
	byKind map[string]metav1.APIResource
	// fetched is when this entry was read from the API server, for the TTL.
	fetched time.Time
}

// resourceTTL bounds how long a group-version's resource list is trusted.
//
// Discovery already retries once through an invalidation on a *miss*, so a
// newly created custom resource resolves without a restart. The opposite
// direction had no answer: a list fetched successfully was never refetched, so
// a kind that was deleted from the cluster kept being offered and IMPORT kept
// generating a table for it. Observed by uninstalling an operator and
// re-importing, whose four kinds came back until the gateway was restarted.
//
// Five minutes is chosen against what it costs on each side. Discovery sits on
// the path of every Get, List and write, so the cache cannot be short-lived;
// one refetch per group-version per five minutes is negligible against that.
// On the other side, a kind that has genuinely gone stops being offered within
// five minutes instead of never, which is the difference between a puzzle and
// a wait.
// DefaultResourceTTL is the default; -discovery-ttl overrides it.
const DefaultResourceTTL = 5 * time.Minute

// Discovery is the cluster-backed Mapper.
//
// Lookups are served from an in-process cache of the discovery document, which
// matters because Resolve sits on the path of every Get/List/Create/Update/
// Delete/Subscribe. A miss invalidates the cache and retries exactly once, so a
// CRD created after the gateway started resolves without a restart while a
// genuinely unknown kind still costs at most one extra round-trip.
type Discovery struct {
	disco discovery.CachedDiscoveryInterface
	// openapi is the source of a kind's top-level field names. Held separately
	// because a cluster can serve discovery while its OpenAPI v3 endpoint is
	// unavailable, and only Describe/Kinds need it.
	openapi openapi.Client
	allow   Allowlist
	// log records each OpenAPI document fetch, so the E2E can assert that a
	// cluster-wide import pays for one per group-version rather than one per
	// kind. nil discards.
	log *slog.Logger
	// access decides whether the gateway's identity may actually list a kind.
	// RBAC, not the allowlist, is the real boundary; the allowlist narrows
	// further for deployments that want to hide kinds they could read.
	access AccessChecker

	// now is time.Now, overridden in tests so the resource TTL can be
	// exercised without waiting for it.
	now func() time.Time
	// resourceTTL is how long a fetched resource list is trusted.
	resourceTTL time.Duration

	openapiFetches atomic.Uint64

	// seenMu guards seenGroupVersions, and is held only for a map insert or a
	// length read. Deliberately not schemaMu: that one is held across the
	// OpenAPI fetch itself, so reporting a statistic under it would let a
	// stats poll block behind a slow network call.
	seenMu sync.Mutex
	// seenGroupVersions is every group-version whose document has been fetched
	// and parsed successfully, ever. A count of the live schemaCache would be
	// a gauge, not the counter StatsResponse promises: forgetSchema drops an
	// entry before a refetch, so a failed refresh would make the reported
	// number go backwards.
	seenGroupVersions map[schema.GroupVersion]struct{}

	mu    sync.RWMutex
	cache map[schema.GroupVersion]resourceCacheEntry

	// schemaMu guards schemaCache and pathsCache. Separate from mu because a
	// schema fetch is slow and must not block resource lookups, which sit on
	// the scan path.
	schemaMu sync.Mutex
	// schemaCache maps a group-version to the top-level properties of every
	// kind it describes, parsed once. Without it, enumerating a cluster
	// re-fetches and re-parses each group-version's document once per kind in
	// it: 65 listable kinds across 13 group-versions on a bare cluster, with
	// the core/v1 document alone at 1.6 MB.
	schemaCache map[schema.GroupVersion]map[schema.GroupVersionKind][]string
	// pathsCache is the /openapi/v3 index, which client-go refetches on every
	// Paths() call.
	pathsCache map[string]openapi.GroupVersion
}

// SetResourceTTL overrides how long a group-version's resource list is
// trusted. Zero or negative leaves the default in place.
func (d *Discovery) SetResourceTTL(ttl time.Duration) {
	if ttl > 0 {
		d.resourceTTL = ttl
	}
}

// NewDiscovery builds a Mapper over a cached discovery client, serving only
// what allow permits.
func NewDiscovery(disco discovery.CachedDiscoveryInterface, allow Allowlist, access AccessChecker, logger *slog.Logger) *Discovery {
	if access == nil {
		access = AllowAll{}
	}
	if logger == nil {
		logger = slog.New(slog.NewTextHandler(io.Discard, nil))
	}
	return &Discovery{
		now:         time.Now,
		resourceTTL: DefaultResourceTTL,
		log:         logger,
		disco:       disco,
		access:      access,
		openapi:     disco.OpenAPIV3(),
		allow:       allow,
		cache:       make(map[schema.GroupVersion]resourceCacheEntry),
		schemaCache: make(map[schema.GroupVersion]map[schema.GroupVersionKind][]string),
	}
}

// resourcesFor returns the cached resources of one group-version, fetching and
// caching on a miss. refresh forces a re-fetch and drops the discovery client's
// own cache first.
func (d *Discovery) resourcesFor(gv schema.GroupVersion, refresh bool) (resourceCacheEntry, error) {
	if !refresh {
		d.mu.RLock()
		e, ok := d.cache[gv]
		d.mu.RUnlock()
		// Expired entries are refetched rather than served. Without this a
		// removed kind is offered for the life of the process.
		if ok && d.now().Sub(e.fetched) < d.resourceTTL {
			return e, nil
		}
		if ok {
			// Stale: drop the discovery client's own cache too, or the
			// refetch below is answered from it and nothing changes.
			d.disco.Invalidate()
		}
	} else {
		d.disco.Invalidate()
		d.mu.Lock()
		delete(d.cache, gv)
		d.mu.Unlock()
	}

	list, err := d.disco.ServerResourcesForGroupVersion(gv.String())
	if err != nil {
		return resourceCacheEntry{}, fmt.Errorf("discovery for %s: %w", gv.String(), err)
	}
	e := resourceCacheEntry{
		byKind:  make(map[string]metav1.APIResource, len(list.APIResources)),
		fetched: d.now(),
	}
	for _, r := range list.APIResources {
		// Subresources ("pods/log", "widgets/status") are addressed through
		// their parent and are never tables of their own.
		if strings.Contains(r.Name, "/") {
			continue
		}
		e.byKind[r.Kind] = r
	}
	d.mu.Lock()
	d.cache[gv] = e
	d.mu.Unlock()
	return e, nil
}

// lookup finds the APIResource for a GVK, retrying once through a cache
// invalidation so a newly created CRD resolves without a gateway restart.
func (d *Discovery) lookup(gvk schema.GroupVersionKind) (metav1.APIResource, error) {
	gv := gvk.GroupVersion()
	for _, refresh := range []bool{false, true} {
		e, err := d.resourcesFor(gv, refresh)
		if err != nil {
			// A group-version the cluster does not have is not a transport
			// failure; report it as an unsupported kind after the retry.
			if refresh {
				return metav1.APIResource{}, unservedKind(gvk)
			}
			continue
		}
		if r, ok := e.byKind[gvk.Kind]; ok {
			return r, nil
		}
	}
	return metav1.APIResource{}, unservedKind(gvk)
}

// unservedKind is the error for a kind this gateway will not serve.
//
// Deliberately one message for three different causes: the cluster has no such
// kind, the gateway's --serve list excludes it, or its RBAC does not permit
// it. Which one it is must not leak, because the allowlist and the RBAC are
// deployment decisions rather than facts about the cluster that a caller may
// enumerate -- distinguishing them would turn any foreign table into a probe
// for what exists.
//
// What it can do is tell the operator where to look. A table that worked
// yesterday and fails today almost always means the gateway's configuration
// changed, and foreign tables are catalog objects that do not move when it
// does, so the table has to be re-imported.
func unservedKind(gvk schema.GroupVersionKind) error {
	return fmt.Errorf(
		"%w: %s. The gateway does not serve this kind: either the cluster has no "+
			"such kind, or the gateway's --serve list or its RBAC excludes it. If "+
			"it was served before, the gateway's configuration changed; restart it "+
			"and re-run IMPORT FOREIGN SCHEMA, because existing foreign tables do "+
			"not follow that change",
		ErrUnsupportedKind, gvk.String())
}

// Resolve implements Mapper.
func (d *Discovery) Resolve(_ context.Context, gvk schema.GroupVersionKind) (schema.GroupVersionResource, bool, error) {
	r, err := d.lookup(gvk)
	if err != nil {
		return schema.GroupVersionResource{}, false, err
	}
	if !d.allow.Permits(gvk.Group, r.Name) {
		// Same error as "no such kind": the allowlist is a deployment
		// decision, not a fact about the cluster a caller may enumerate.
		return schema.GroupVersionResource{}, false, unservedKind(gvk)
	}
	return gvk.GroupVersion().WithResource(r.Name), r.Namespaced, nil
}

// hasVerb reports whether verbs contains want.
func hasVerb(verbs metav1.Verbs, want string) bool {
	for _, v := range verbs {
		if v == want {
			return true
		}
	}
	return false
}

// kindInfo assembles a KindInfo from an APIResource plus its schema fields.
func kindInfo(gvk schema.GroupVersionKind, r metav1.APIResource, topLevel []string) KindInfo {
	return KindInfo{
		GVK:        gvk,
		Plural:     r.Name,
		Namespaced: r.Namespaced,
		Columns:    Columns(gvk, r.Namespaced, topLevel),
		Writable: hasVerb(r.Verbs, "create") &&
			hasVerb(r.Verbs, "update") &&
			hasVerb(r.Verbs, "delete"),
		Watchable: hasVerb(r.Verbs, "watch"),
	}
}

// Describe implements Mapper.
func (d *Discovery) Describe(ctx context.Context, gvk schema.GroupVersionKind) (KindInfo, error) {
	r, err := d.lookup(gvk)
	if err != nil {
		return KindInfo{}, err
	}
	if !d.allow.Permits(gvk.Group, r.Name) {
		return KindInfo{}, unservedKind(gvk)
	}
	// A kind the gateway cannot list has no rows, and generating a table for
	// it would turn an RBAC gap into a confusing runtime error on every scan.
	gvr := gvk.GroupVersion().WithResource(r.Name)
	allowed, err := d.access.CanList(ctx, gvr)
	if err != nil {
		return KindInfo{}, err
	}
	if !allowed {
		return KindInfo{}, unservedKind(gvk)
	}
	topLevel, err := d.topLevelFields(ctx, gvk)
	if err != nil {
		return KindInfo{}, err
	}
	return kindInfo(gvk, r, topLevel), nil
}

// candidate is a kind that passed the allowlist, pending an access check.
type candidate struct {
	gvk schema.GroupVersionKind
	res metav1.APIResource
}

// maxAccessConcurrency bounds in-flight SelfSubjectAccessReviews. High enough
// that ~65 checks complete quickly, low enough not to burst the API server's
// priority-and-fairness budget on a shared control plane.
const maxAccessConcurrency = 8

// filterByAccess returns the candidates the gateway may list, preserving order.
//
// A check that fails to complete aborts the enumeration rather than dropping
// the kind: silently omitting a table because the authorization API blipped
// would look identical to the kind not existing (docs/RULES.md §1).
func (d *Discovery) filterByAccess(ctx context.Context, in []candidate) ([]candidate, error) {
	keep := make([]bool, len(in))
	errs := make([]error, len(in))
	sem := make(chan struct{}, maxAccessConcurrency)
	var wg sync.WaitGroup
	for i, c := range in {
		wg.Add(1)
		go func(i int, c candidate) {
			defer wg.Done()
			sem <- struct{}{}
			defer func() { <-sem }()
			gvr := c.gvk.GroupVersion().WithResource(c.res.Name)
			keep[i], errs[i] = d.access.CanList(ctx, gvr)
		}(i, c)
	}
	wg.Wait()

	out := make([]candidate, 0, len(in))
	for i := range in {
		if errs[i] != nil {
			return nil, errs[i]
		}
		if keep[i] {
			out = append(out, in[i])
		}
	}
	return out, nil
}

// Kinds implements Mapper.
//
// A kind inside the allowlist whose schema cannot be read is omitted rather
// than failing the whole enumeration, so one malformed CRD cannot break
// IMPORT FOREIGN SCHEMA for every other kind. DiscoverSchema on that kind
// still reports the underlying error.
func (d *Discovery) Kinds(ctx context.Context, group *string, plurals []string) ([]KindInfo, error) {
	want := make(map[string]struct{}, len(plurals))
	for _, p := range plurals {
		want[strings.ToLower(p)] = struct{}{}
	}

	groups, err := d.disco.ServerGroups()
	if err != nil {
		return nil, fmt.Errorf("discovery: server groups: %w", err)
	}

	var out []KindInfo
	var candidates []candidate
	for _, g := range groups.Groups {
		if group != nil && g.Name != *group {
			continue
		}
		// Only the group's preferred version: offering every served version of
		// a kind would generate colliding table names for no benefit.
		gv, err := schema.ParseGroupVersion(g.PreferredVersion.GroupVersion)
		if err != nil {
			continue
		}
		e, err := d.resourcesFor(gv, false)
		if err != nil {
			continue
		}
		for kind, r := range e.byKind {
			if !d.allow.Permits(gv.Group, r.Name) {
				continue
			}
			if len(want) > 0 {
				if _, ok := want[strings.ToLower(r.Name)]; !ok {
					continue
				}
			}
			if !hasVerb(r.Verbs, "list") {
				// Not a table: a kind that cannot be listed has no rows.
				continue
			}
			candidates = append(candidates, candidate{gvk: gv.WithKind(kind), res: r})
		}
	}

	// Ask the API server which of these the gateway may actually list. One
	// review per kind, bounded concurrency: a cluster has ~65 listable kinds
	// and serial round-trips would make an import feel broken.
	permitted, err := d.filterByAccess(ctx, candidates)
	if err != nil {
		return nil, err
	}
	for _, c := range permitted {
		topLevel, err := d.topLevelFields(ctx, c.gvk)
		if err != nil {
			continue
		}
		out = append(out, kindInfo(c.gvk, c.res, topLevel))
	}
	sort.Slice(out, func(i, j int) bool {
		if out[i].GVK.Group != out[j].GVK.Group {
			return out[i].GVK.Group < out[j].GVK.Group
		}
		return out[i].Plural < out[j].Plural
	})
	return out, nil
}

// openAPIDoc is the slice of an OpenAPI v3 document this package reads: the
// component schemas, each tagged with the GVKs it describes.
type openAPIDoc struct {
	Components struct {
		Schemas map[string]struct {
			Properties map[string]json.RawMessage `json:"properties"`
			GVKs       []struct {
				Group   string `json:"group"`
				Version string `json:"version"`
				Kind    string `json:"kind"`
			} `json:"x-kubernetes-group-version-kind"`
		} `json:"schemas"`
	} `json:"components"`
}

// openAPIPath is the OpenAPI v3 path key for a group-version. The core group
// lives under "api/v1"; everything else under "apis/<group>/<version>".
func openAPIPath(gv schema.GroupVersion) string {
	if gv.Group == "" {
		return "api/" + gv.Version
	}
	return "apis/" + gv.Group + "/" + gv.Version
}

// paths returns the /openapi/v3 index, fetched at most once.
//
// client-go's Paths() issues a request every call, so without this a
// cluster-wide enumeration pays for the index once per kind on top of the
// documents themselves. Caller must hold schemaMu.
func (d *Discovery) paths() (map[string]openapi.GroupVersion, error) {
	if d.pathsCache != nil {
		return d.pathsCache, nil
	}
	p, err := d.openapi.Paths()
	if err != nil {
		return nil, fmt.Errorf("openapi: paths: %w", err)
	}
	d.pathsCache = p
	return p, nil
}

// forgetSchema drops a group-version's parsed document and the paths index, so
// the next lookup refetches both. Caller must hold schemaMu.
func (d *Discovery) forgetSchema(gv schema.GroupVersion) {
	delete(d.schemaCache, gv)
	// The index is dropped too: a group-version that did not exist when the
	// index was fetched has no entry in it, so keeping it would make the
	// refetch fail to find the document at all.
	d.pathsCache = nil
}

// schemaFor returns every kind described by one group-version's OpenAPI
// document, mapped to its top-level property names, parsing the document once.
//
// Kinds are located by their x-kubernetes-group-version-kind extension rather
// than by the component key's Go-package-derived spelling, which differs
// between built-ins ("io.k8s.api.core.v1.Pod") and CRDs and is not a stable
// contract. Caller must hold schemaMu.
func (d *Discovery) schemaFor(gv schema.GroupVersion) (map[schema.GroupVersionKind][]string, error) {
	if cached, ok := d.schemaCache[gv]; ok {
		return cached, nil
	}
	paths, err := d.paths()
	if err != nil {
		return nil, err
	}
	page, ok := paths[openAPIPath(gv)]
	if !ok {
		return nil, fmt.Errorf("openapi: no schema document for %s", gv.String())
	}
	raw, err := page.Schema("application/json")
	if err != nil {
		return nil, fmt.Errorf("openapi: schema for %s: %w", gv.String(), err)
	}
	var doc openAPIDoc
	if err := json.Unmarshal(raw, &doc); err != nil {
		return nil, fmt.Errorf("openapi: parse schema for %s: %w", gv.String(), err)
	}
	d.openapiFetches.Add(1)
	d.seenMu.Lock()
	if d.seenGroupVersions == nil {
		d.seenGroupVersions = make(map[schema.GroupVersion]struct{})
	}
	d.seenGroupVersions[gv] = struct{}{}
	d.seenMu.Unlock()
	d.log.Info("openapi_fetch",
		slog.String("group_version", gv.String()),
		slog.Int("bytes", len(raw)),
		slog.Int("schemas", len(doc.Components.Schemas)))
	out := make(map[schema.GroupVersionKind][]string, len(doc.Components.Schemas))
	for _, sch := range doc.Components.Schemas {
		fields := make([]string, 0, len(sch.Properties))
		for name := range sch.Properties {
			fields = append(fields, name)
		}
		sort.Strings(fields)
		for _, t := range sch.GVKs {
			out[schema.GroupVersionKind{Group: t.Group, Version: t.Version, Kind: t.Kind}] = fields
		}
	}
	d.schemaCache[gv] = out
	return out, nil
}

// OpenAPIFetches reports the total number of OpenAPI v3 document fetches performed.
func (d *Discovery) OpenAPIFetches() uint64 {
	return d.openapiFetches.Load()
}

// OpenAPIGroupVersions reports how many distinct group-versions have had their
// OpenAPI document fetched and parsed, counted over the process's lifetime.
//
// Monotonic, matching the rest of StatsResponse. The live schemaCache would
// not be: forgetSchema drops an entry before refetching, so a failed refresh
// would make a caller see the number fall. Comparing it against
// OpenAPIFetches is the whole point -- one fetch per group-version rather than
// one per kind -- and that comparison needs both sides counted the same way.
func (d *Discovery) OpenAPIGroupVersions() uint64 {
	d.seenMu.Lock()
	defer d.seenMu.Unlock()
	return uint64(len(d.seenGroupVersions))
}

// AccessReviews reports the total number of SelfSubjectAccessReview calls issued,
// if the underlying AccessChecker tracks reviews.
func (d *Discovery) AccessReviews() uint64 {
	if ar, ok := d.access.(interface{ AccessReviews() uint64 }); ok {
		return ar.AccessReviews()
	}
	return 0
}

// topLevelFields returns the names of a kind's own top-level schema properties,
// served from the group-version's parsed document.
func (d *Discovery) topLevelFields(_ context.Context, gvk schema.GroupVersionKind) ([]string, error) {
	if d.openapi == nil {
		return nil, fmt.Errorf("openapi: gateway has no OpenAPI client configured")
	}
	d.schemaMu.Lock()
	defer d.schemaMu.Unlock()

	gv := gvk.GroupVersion()
	// Retry once through a cache drop, mirroring what resource discovery does
	// on a miss. A CRD created after this group-version's document was parsed
	// resolves through the RESTMapper but is absent from the cached schema, so
	// without this it could not be described until the gateway restarted --
	// which would undo the "a new CRD needs no restart" property for the
	// describe path while leaving it true for Resolve.
	for _, refresh := range []bool{false, true} {
		if refresh {
			d.forgetSchema(gv)
		}
		byKind, err := d.schemaFor(gv)
		if err != nil {
			if refresh {
				return nil, err
			}
			continue
		}
		if fields, ok := byKind[gvk]; ok {
			return fields, nil
		}
	}
	return nil, fmt.Errorf("openapi: %s is not described by the schema document for %s", gvk.Kind, gv.String())
}
