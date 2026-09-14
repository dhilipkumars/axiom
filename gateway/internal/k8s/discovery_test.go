package k8s

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"
	"k8s.io/apimachinery/pkg/runtime/schema"
	"k8s.io/client-go/discovery"
	"k8s.io/client-go/openapi"
)

// --- fakes -------------------------------------------------------------------
//
// client-go's own discovery fake does not serve OpenAPI v3, and the interfaces
// this package actually uses are small, so the doubles live here. Methods that
// Discovery must never call are left to the embedded nil interface, which
// panics loudly if that assumption breaks.

type fakeGroupVersion struct {
	doc []byte
	err error
	// fetches counts Schema() calls, to prove each document is parsed once.
	fetches *atomic.Int32
}

func (f fakeGroupVersion) Schema(string) ([]byte, error) {
	if f.fetches != nil {
		f.fetches.Add(1)
	}
	return f.doc, f.err
}
func (f fakeGroupVersion) ServerRelativeURL() string { return "" }

type fakeOpenAPI struct {
	paths map[string]openapi.GroupVersion
	err   error
	// calls counts Paths(), which client-go refetches every time.
	calls *atomic.Int32
}

func (f fakeOpenAPI) Paths() (map[string]openapi.GroupVersion, error) {
	if f.calls != nil {
		f.calls.Add(1)
	}
	if f.err != nil {
		return nil, f.err
	}
	return f.paths, nil
}

type fakeDiscovery struct {
	discovery.CachedDiscoveryInterface // nil: any unexpected call panics

	groups *metav1.APIGroupList
	// byGV is the resource list per "group/version" key; a missing key is an
	// error, as the API server reports for a group-version it does not have.
	byGV map[string]*metav1.APIResourceList

	// calls counts ServerResourcesForGroupVersion, to prove caching.
	calls atomic.Int32
	// invalidations counts Invalidate, to prove the retry-once path.
	invalidations atomic.Int32
	openapi       openapi.Client

	// OpenAPI fetch counters, asserted by the caching tests.
	coreFetches *atomic.Int32
	crdFetches  *atomic.Int32
	pathsCalls  *atomic.Int32
}

func (f *fakeDiscovery) ServerResourcesForGroupVersion(gv string) (*metav1.APIResourceList, error) {
	f.calls.Add(1)
	list, ok := f.byGV[gv]
	if !ok {
		return nil, fmt.Errorf("the server could not find the requested resource: %s", gv)
	}
	return list, nil
}

func (f *fakeDiscovery) ServerGroups() (*metav1.APIGroupList, error) {
	if f.groups == nil {
		return nil, errors.New("no groups")
	}
	return f.groups, nil
}

func (f *fakeDiscovery) Invalidate()               { f.invalidations.Add(1) }
func (f *fakeDiscovery) OpenAPIV3() openapi.Client { return f.openapi }

// openAPIDocFor builds a minimal OpenAPI v3 document describing kinds by their
// x-kubernetes-group-version-kind extension, with the given top-level fields.
func openAPIDocFor(t *testing.T, kinds map[schema.GroupVersionKind][]string) []byte {
	t.Helper()
	type prop = map[string]any
	schemas := map[string]any{}
	for gvk, fields := range kinds {
		props := prop{}
		for _, f := range fields {
			props[f] = prop{"type": "object"}
		}
		schemas[gvk.Group+"."+gvk.Version+"."+gvk.Kind] = prop{
			"properties": props,
			"x-kubernetes-group-version-kind": []prop{
				{"group": gvk.Group, "version": gvk.Version, "kind": gvk.Kind},
			},
		}
	}
	// A decoy schema for an unrelated kind, so lookup by GVK is actually tested.
	schemas["decoy"] = prop{
		"properties": prop{"nonsense": prop{}},
		"x-kubernetes-group-version-kind": []prop{
			{"group": "decoy.io", "version": "v9", "kind": "Decoy"},
		},
	}
	doc, err := json.Marshal(prop{"components": prop{"schemas": schemas}})
	if err != nil {
		t.Fatal(err)
	}
	return doc
}

var (
	widgetGVK  = schema.GroupVersionKind{Group: "example.com", Version: "v1", Kind: "Widget"}
	clusterGVK = schema.GroupVersionKind{Group: "example.com", Version: "v1", Kind: "ClusterWidget"}
)

// newTestDiscovery wires a Discovery over fakes serving pods plus an
// example.com group with a namespaced and a cluster-scoped CRD, permitting
// every kind. Tests about RBAC filtering use newTestDiscoveryWithAccess.
func newTestDiscovery(t *testing.T, serve string) (*Discovery, *fakeDiscovery) {
	t.Helper()
	d, fd := newTestDiscoveryWithAccess(t, serve, AllowAll{})
	return d, fd
}

// fakeAccess permits exactly the resource names it is given.
type fakeAccess struct {
	allowed map[string]bool
	err     error
	// checks counts CanList calls, to prove they are not made per scan.
	checks atomic.Int32
}

func (f *fakeAccess) CanList(_ context.Context, gvr schema.GroupVersionResource) (bool, error) {
	f.checks.Add(1)
	if f.err != nil {
		return false, f.err
	}
	return f.allowed[gvr.Resource], nil
}

func newTestDiscoveryWithAccess(t *testing.T, serve string, access AccessChecker) (*Discovery, *fakeDiscovery) {
	t.Helper()
	allow, err := ParseAllowlist(serve)
	if err != nil {
		t.Fatal(err)
	}
	fd := &fakeDiscovery{
		groups: &metav1.APIGroupList{Groups: []metav1.APIGroup{
			{Name: "", PreferredVersion: metav1.GroupVersionForDiscovery{GroupVersion: "v1"}},
			{Name: "example.com", PreferredVersion: metav1.GroupVersionForDiscovery{GroupVersion: "example.com/v1"}},
		}},
		byGV: map[string]*metav1.APIResourceList{
			"v1": {APIResources: []metav1.APIResource{
				{Name: "pods", Kind: "Pod", Namespaced: true, Verbs: metav1.Verbs{"get", "list", "watch", "create", "update", "delete"}},
				{Name: "pods/log", Kind: "Pod", Namespaced: true, Verbs: metav1.Verbs{"get"}},
				{Name: "secrets", Kind: "Secret", Namespaced: true, Verbs: metav1.Verbs{"get", "list", "watch"}},
			}},
			"example.com/v1": {APIResources: []metav1.APIResource{
				{Name: "widgets", Kind: "Widget", Namespaced: true, Verbs: metav1.Verbs{"get", "list", "watch", "create", "update", "delete"}},
				{Name: "clusterwidgets", Kind: "ClusterWidget", Namespaced: false, Verbs: metav1.Verbs{"get", "list"}},
			}},
		},
	}
	fd.coreFetches = &atomic.Int32{}
	fd.crdFetches = &atomic.Int32{}
	fd.pathsCalls = &atomic.Int32{}
	fd.openapi = fakeOpenAPI{calls: fd.pathsCalls, paths: map[string]openapi.GroupVersion{
		"api/v1": fakeGroupVersion{fetches: fd.coreFetches, doc: openAPIDocFor(t, map[schema.GroupVersionKind][]string{
			{Version: "v1", Kind: "Pod"}:    {"apiVersion", "kind", "metadata", "spec", "status"},
			{Version: "v1", Kind: "Secret"}: {"apiVersion", "kind", "metadata", "data", "stringData", "type"},
		})},
		"apis/example.com/v1": fakeGroupVersion{fetches: fd.crdFetches, doc: openAPIDocFor(t, map[schema.GroupVersionKind][]string{
			widgetGVK:  {"apiVersion", "kind", "metadata", "spec", "status"},
			clusterGVK: {"apiVersion", "kind", "metadata", "spec"},
		})},
	}}
	return NewDiscovery(fd, allow, access, nil), fd
}

// --- tests -------------------------------------------------------------------

func TestDiscoveryResolvesACRD(t *testing.T) {
	t.Parallel()
	d, _ := newTestDiscovery(t, "pods,widgets.example.com")
	gvr, namespaced, err := d.Resolve(context.Background(), widgetGVK)
	if err != nil {
		t.Fatalf("Resolve(Widget) = %v", err)
	}
	if gvr.Resource != "widgets" || gvr.Group != "example.com" || !namespaced {
		t.Errorf("Resolve(Widget) = %v namespaced=%v", gvr, namespaced)
	}
}

func TestDiscoveryRespectsTheAllowlist(t *testing.T) {
	t.Parallel()
	// secrets exist in the cluster but are outside the serve list.
	d, _ := newTestDiscovery(t, "pods")
	_, _, err := d.Resolve(context.Background(), schema.GroupVersionKind{Version: "v1", Kind: "Secret"})
	if !errors.Is(err, ErrUnsupportedKind) {
		t.Fatalf("Resolve(Secret) = %v, want ErrUnsupportedKind", err)
	}
	// The error must not distinguish "not allowed" from "does not exist", or
	// the allowlist becomes enumerable: anyone able to define a foreign table
	// could probe for what the cluster holds.
	_, _, missing := d.Resolve(context.Background(), schema.GroupVersionKind{Version: "v1", Kind: "Nonexistent"})
	if !errors.Is(missing, ErrUnsupportedKind) {
		t.Fatalf("Resolve(Nonexistent) = %v, want ErrUnsupportedKind", missing)
	}
	// Compare the two messages directly, with the kind removed, rather than
	// looking for words like "serve" in one of them. The property is that the
	// two are indistinguishable, and a substring check is only a proxy for it:
	// it rejects a message that names every possible cause, which leaks
	// nothing because it is the same message either way, while it would accept
	// two differently-worded messages that leak everything.
	denied := strings.ReplaceAll(err.Error(), "Secret", "KIND")
	absent := strings.ReplaceAll(missing.Error(), "Nonexistent", "KIND")
	if denied != absent {
		t.Errorf("the two errors differ, so the allowlist is enumerable:\n denied: %s\n absent: %s", denied, absent)
	}
}

func TestDiscoveryIgnoresSubresources(t *testing.T) {
	t.Parallel()
	d, _ := newTestDiscovery(t, "pods,*.example.com")
	gvr, _, err := d.Resolve(context.Background(), schema.GroupVersionKind{Version: "v1", Kind: "Pod"})
	if err != nil {
		t.Fatal(err)
	}
	// "pods/log" shares the Pod kind and must never win the lookup.
	if gvr.Resource != "pods" {
		t.Errorf("Resolve(Pod) = %q, want pods", gvr.Resource)
	}
}

func TestDiscoveryCachesAndRetriesOnceOnAMiss(t *testing.T) {
	t.Parallel()
	d, fd := newTestDiscovery(t, "pods")
	ctx := context.Background()
	for range 5 {
		if _, _, err := d.Resolve(ctx, schema.GroupVersionKind{Version: "v1", Kind: "Pod"}); err != nil {
			t.Fatal(err)
		}
	}
	if got := fd.calls.Load(); got != 1 {
		t.Errorf("discovery fetched %d times for 5 resolves, want 1: Resolve is on every scan's path", got)
	}

	// An unknown kind in a known group-version must not invalidate: the
	// group-version was fetched successfully, the kind simply is not in it.
	before := fd.invalidations.Load()
	_, _, err := d.Resolve(ctx, schema.GroupVersionKind{Version: "v1", Kind: "Ghost"})
	if !errors.Is(err, ErrUnsupportedKind) {
		t.Fatalf("err = %v, want ErrUnsupportedKind", err)
	}
	if got := fd.invalidations.Load() - before; got != 1 {
		t.Errorf("invalidated %d times, want exactly 1 (retry once so a new CRD resolves without a restart)", got)
	}
}

func TestDiscoveryFindsACRDCreatedAfterStartup(t *testing.T) {
	t.Parallel()
	d, fd := newTestDiscovery(t, "gadgets.example.com")
	ctx := context.Background()
	gadget := schema.GroupVersionKind{Group: "example.com", Version: "v1", Kind: "Gadget"}
	if _, _, err := d.Resolve(ctx, gadget); !errors.Is(err, ErrUnsupportedKind) {
		t.Fatalf("before creation: err = %v, want ErrUnsupportedKind", err)
	}
	// The CRD appears in the cluster after the gateway cached the group.
	fd.byGV["example.com/v1"].APIResources = append(fd.byGV["example.com/v1"].APIResources,
		metav1.APIResource{Name: "gadgets", Kind: "Gadget", Namespaced: true, Verbs: metav1.Verbs{"get", "list"}})
	gvr, _, err := d.Resolve(ctx, gadget)
	if err != nil {
		t.Fatalf("after creation: %v (a CRD created while the gateway runs must resolve without a restart)", err)
	}
	if gvr.Resource != "gadgets" {
		t.Errorf("Resolve(Gadget) = %q, want gadgets", gvr.Resource)
	}
}

func TestDiscoveryDescribeBuildsColumnsFromOpenAPI(t *testing.T) {
	t.Parallel()
	d, _ := newTestDiscovery(t, "*.example.com")
	info, err := d.Describe(context.Background(), widgetGVK)
	if err != nil {
		t.Fatalf("Describe(Widget) = %v", err)
	}
	if info.Plural != "widgets" || !info.Namespaced {
		t.Errorf("Describe(Widget) plural=%q namespaced=%v", info.Plural, info.Namespaced)
	}
	if !info.Writable || !info.Watchable {
		t.Errorf("Widget advertises create/update/delete/watch; got writable=%v watchable=%v", info.Writable, info.Watchable)
	}
	want := "api_version,kind,name,namespace,uid,resource_version,creation_timestamp,labels,annotations,metadata,spec,status,raw"
	if got := joined(info.Columns); got != want {
		t.Errorf("columns = %s\n     want %s", got, want)
	}
}

func TestDiscoveryDescribeClusterScopedKind(t *testing.T) {
	t.Parallel()
	d, _ := newTestDiscovery(t, "*.example.com")
	info, err := d.Describe(context.Background(), clusterGVK)
	if err != nil {
		t.Fatal(err)
	}
	if info.Namespaced {
		t.Error("ClusterWidget should be cluster-scoped")
	}
	if strings.Contains(joined(info.Columns), "namespace") {
		t.Errorf("cluster-scoped kind must have no namespace column, got %s", joined(info.Columns))
	}
	if info.Writable {
		t.Error("ClusterWidget advertises only get/list; Writable must be false")
	}
	if info.Watchable {
		t.Error("ClusterWidget does not advertise watch; Watchable must be false")
	}
}

func TestDiscoveryDescribeFailsRatherThanReturningAThinTable(t *testing.T) {
	t.Parallel()
	d, fd := newTestDiscovery(t, "pods")
	fd.openapi = fakeOpenAPI{err: errors.New("openapi endpoint unavailable")}
	d.openapi = fd.openapi
	_, err := d.Describe(context.Background(), schema.GroupVersionKind{Version: "v1", Kind: "Pod"})
	if err == nil {
		t.Fatal("Describe must fail when the schema cannot be read, not return a kind with no fields")
	}
	if !strings.Contains(err.Error(), "openapi") {
		t.Errorf("err = %v, want it to name openapi as the cause", err)
	}
}

func TestDiscoveryDescribeUnknownKindInSchemaDocument(t *testing.T) {
	t.Parallel()
	d, _ := newTestDiscovery(t, "secrets")
	// Secrets resolve, and the document describes them; flip to a kind the
	// document does not describe to exercise the not-in-schema path.
	d.openapi = fakeOpenAPI{paths: map[string]openapi.GroupVersion{
		"api/v1": fakeGroupVersion{doc: openAPIDocFor(t, nil)},
	}}
	_, err := d.Describe(context.Background(), schema.GroupVersionKind{Version: "v1", Kind: "Secret"})
	if err == nil || !strings.Contains(err.Error(), "not described") {
		t.Fatalf("err = %v, want a 'not described by the schema document' error", err)
	}
}

func TestDiscoveryKindsEnumeratesOnlyServedKinds(t *testing.T) {
	t.Parallel()
	d, _ := newTestDiscovery(t, "pods,*.example.com")
	got, err := d.Kinds(context.Background(), nil, nil)
	if err != nil {
		t.Fatal(err)
	}
	var plurals []string
	for _, k := range got {
		plurals = append(plurals, k.Plural)
	}
	// Sorted by group then plural; secrets are excluded by the allowlist.
	want := "pods,clusterwidgets,widgets"
	if strings.Join(plurals, ",") != want {
		t.Errorf("Kinds() = %v, want %s", plurals, want)
	}
}

func TestDiscoveryKindsFilters(t *testing.T) {
	t.Parallel()
	d, _ := newTestDiscovery(t, "pods,*.example.com")
	ctx := context.Background()

	group := "example.com"
	byGroup, err := d.Kinds(ctx, &group, nil)
	if err != nil {
		t.Fatal(err)
	}
	for _, k := range byGroup {
		if k.GVK.Group != "example.com" {
			t.Errorf("group filter leaked %s", k.GVK)
		}
	}
	if len(byGroup) != 2 {
		t.Errorf("group filter returned %d kinds, want 2", len(byGroup))
	}

	core := ""
	byCore, err := d.Kinds(ctx, &core, nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(byCore) != 1 || byCore[0].Plural != "pods" {
		t.Errorf("core group filter = %v, want just pods (the empty group must mean core, not unset)", byCore)
	}

	byPlural, err := d.Kinds(ctx, nil, []string{"widgets", "nonexistent"})
	if err != nil {
		t.Fatal(err)
	}
	if len(byPlural) != 1 || byPlural[0].Plural != "widgets" {
		t.Errorf("plural filter = %v, want just widgets; an unmatched name is omitted, not an error", byPlural)
	}
}

func TestDiscoveryKindsSkipsUnlistableKinds(t *testing.T) {
	t.Parallel()
	d, fd := newTestDiscovery(t, "*.example.com")
	fd.byGV["example.com/v1"].APIResources = append(fd.byGV["example.com/v1"].APIResources,
		metav1.APIResource{Name: "actions", Kind: "Action", Namespaced: true, Verbs: metav1.Verbs{"create"}})
	got, err := d.Kinds(context.Background(), nil, nil)
	if err != nil {
		t.Fatal(err)
	}
	for _, k := range got {
		if k.Plural == "actions" {
			t.Error("a kind that cannot be listed has no rows and must not become a table")
		}
	}
}

func TestOpenAPIPath(t *testing.T) {
	t.Parallel()
	if got := openAPIPath(schema.GroupVersion{Version: "v1"}); got != "api/v1" {
		t.Errorf("core group path = %q, want api/v1", got)
	}
	if got := openAPIPath(schema.GroupVersion{Group: "example.com", Version: "v1"}); got != "apis/example.com/v1" {
		t.Errorf("group path = %q, want apis/example.com/v1", got)
	}
}

// TestSchemaDocumentsAreFetchedOncePerGroupVersion is the Phase 5 blocker in a
// test. Serving a whole cluster means describing every kind, and client-go
// caches neither Paths() nor Schema(): a bare kind cluster has 65 listable
// kinds across 13 group-versions, with the core/v1 document alone at 1.6 MB, so
// a per-kind fetch re-parses that document once per core kind.
func TestSchemaDocumentsAreFetchedOncePerGroupVersion(t *testing.T) {
	t.Parallel()
	d, fd := newTestDiscovery(t, "pods,secrets,*.example.com")
	ctx := context.Background()

	// Two kinds in core/v1, two in example.com/v1.
	for _, gvk := range []schema.GroupVersionKind{
		{Version: "v1", Kind: "Pod"},
		{Version: "v1", Kind: "Secret"},
		widgetGVK,
		clusterGVK,
	} {
		if _, err := d.Describe(ctx, gvk); err != nil {
			t.Fatalf("Describe(%s) = %v", gvk, err)
		}
	}

	if got := fd.coreFetches.Load(); got != 1 {
		t.Errorf("core/v1 document fetched %d times for 2 kinds, want 1", got)
	}
	if got := fd.crdFetches.Load(); got != 1 {
		t.Errorf("example.com/v1 document fetched %d times for 2 kinds, want 1", got)
	}
	if got := fd.pathsCalls.Load(); got != 1 {
		t.Errorf("/openapi/v3 index fetched %d times, want 1", got)
	}

	// Describing the same kinds again must not refetch either.
	for _, gvk := range []schema.GroupVersionKind{{Version: "v1", Kind: "Pod"}, widgetGVK} {
		if _, err := d.Describe(ctx, gvk); err != nil {
			t.Fatal(err)
		}
	}
	if got := fd.coreFetches.Load() + fd.crdFetches.Load() + fd.pathsCalls.Load(); got != 3 {
		t.Errorf("repeat describes refetched: total fetches = %d, want 3", got)
	}
}

func TestKindsEnumerationFetchesEachDocumentOnce(t *testing.T) {
	t.Parallel()
	d, fd := newTestDiscovery(t, "pods,secrets,*.example.com")
	kinds, err := d.Kinds(context.Background(), nil, nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(kinds) < 4 {
		t.Fatalf("enumerated %d kinds, want at least 4", len(kinds))
	}
	// One fetch per group-version regardless of how many kinds each holds.
	if c, w := fd.coreFetches.Load(), int32(1); c != w {
		t.Errorf("core/v1 fetched %d times during enumeration, want %d", c, w)
	}
	if c, w := fd.crdFetches.Load(), int32(1); c != w {
		t.Errorf("example.com/v1 fetched %d times during enumeration, want %d", c, w)
	}
}

func TestSchemaFetchErrorIsNotCachedAsSuccess(t *testing.T) {
	t.Parallel()
	d, fd := newTestDiscovery(t, "pods")
	// A transient failure must not poison the cache with an empty result, or
	// every later Describe would report the kind as having no fields.
	failing := fakeOpenAPI{calls: fd.pathsCalls, paths: map[string]openapi.GroupVersion{
		"api/v1": fakeGroupVersion{fetches: fd.coreFetches, err: errors.New("boom")},
	}}
	d.openapi = failing
	pod := schema.GroupVersionKind{Version: "v1", Kind: "Pod"}
	if _, err := d.Describe(context.Background(), pod); err == nil {
		t.Fatal("want an error from a failing schema fetch")
	}
	// Recover, and the next call must actually fetch rather than serve a
	// cached failure.
	d.openapi = fakeOpenAPI{calls: fd.pathsCalls, paths: map[string]openapi.GroupVersion{
		"api/v1": fakeGroupVersion{fetches: fd.coreFetches, doc: openAPIDocFor(t, map[schema.GroupVersionKind][]string{
			pod: {"apiVersion", "kind", "metadata", "spec", "status"},
		})},
	}}
	// The paths index was cached from the failed attempt; drop it so the new
	// client is consulted, the way a caller would after reconfiguring.
	d.pathsCache = nil
	info, err := d.Describe(context.Background(), pod)
	if err != nil {
		t.Fatalf("after recovery: %v", err)
	}
	if !strings.Contains(joined(info.Columns), "spec") {
		t.Errorf("recovered describe lost fields: %s", joined(info.Columns))
	}
}

func TestEnumerationFollowsRBACNotJustTheAllowlist(t *testing.T) {
	t.Parallel()
	// The allowlist permits everything; RBAC permits only pods and widgets.
	access := &fakeAccess{allowed: map[string]bool{"pods": true, "widgets": true}}
	d, _ := newTestDiscoveryWithAccess(t, "*,*.example.com", access)

	got, err := d.Kinds(context.Background(), nil, nil)
	if err != nil {
		t.Fatal(err)
	}
	var plurals []string
	for _, k := range got {
		plurals = append(plurals, k.Plural)
	}
	if strings.Join(plurals, ",") != "pods,widgets" {
		t.Errorf("Kinds() = %v, want pods,widgets: secrets and clusterwidgets exist "+
			"and are allowlisted but the identity cannot list them", plurals)
	}
}

func TestDescribeRefusesAKindRBACForbids(t *testing.T) {
	t.Parallel()
	access := &fakeAccess{allowed: map[string]bool{"pods": true}}
	d, _ := newTestDiscoveryWithAccess(t, "*,*.example.com", access)

	if _, err := d.Describe(context.Background(), schema.GroupVersionKind{Version: "v1", Kind: "Pod"}); err != nil {
		t.Fatalf("Describe(Pod) = %v, want success", err)
	}
	// Secrets exist and are allowlisted, but the identity cannot list them.
	// Generating a table would turn an RBAC gap into a runtime error on scan.
	_, err := d.Describe(context.Background(), schema.GroupVersionKind{Version: "v1", Kind: "Secret"})
	if !errors.Is(err, ErrUnsupportedKind) {
		t.Fatalf("Describe(Secret) = %v, want ErrUnsupportedKind", err)
	}
}

func TestAllowlistStillNarrowsBeyondRBAC(t *testing.T) {
	t.Parallel()
	// RBAC permits everything; the allowlist is the narrower of the two.
	d, _ := newTestDiscoveryWithAccess(t, "pods", AllowAll{})
	got, err := d.Kinds(context.Background(), nil, nil)
	if err != nil {
		t.Fatal(err)
	}
	if len(got) != 1 || got[0].Plural != "pods" {
		t.Errorf("Kinds() = %v, want just pods: the allowlist must still be able "+
			"to hide kinds the identity could read", got)
	}
}

func TestAnUnansweredAccessCheckFailsRatherThanHidingKinds(t *testing.T) {
	t.Parallel()
	access := &fakeAccess{err: errors.New("authorization API unavailable")}
	d, _ := newTestDiscoveryWithAccess(t, "*,*.example.com", access)

	_, err := d.Kinds(context.Background(), nil, nil)
	if err == nil {
		t.Fatal("a failed access check must fail the enumeration, not silently " +
			"return fewer kinds: an empty result is indistinguishable from a cluster with nothing in it")
	}
	if !strings.Contains(err.Error(), "authorization") {
		t.Errorf("err = %v, want it to name the cause", err)
	}
}

func TestAccessAnswersAreNotReCheckedPerKindLookup(t *testing.T) {
	t.Parallel()
	access := &fakeAccess{allowed: map[string]bool{"pods": true}}
	d, _ := newTestDiscoveryWithAccess(t, "pods", access)
	ctx := context.Background()
	pod := schema.GroupVersionKind{Version: "v1", Kind: "Pod"}

	// Resolve is on the scan path and must never issue an access review: RBAC
	// is enforced by the API server on the actual request.
	for range 5 {
		if _, _, err := d.Resolve(ctx, pod); err != nil {
			t.Fatal(err)
		}
	}
	if got := access.checks.Load(); got != 0 {
		t.Errorf("Resolve issued %d access reviews; scans must not pay for authorization round-trips", got)
	}
}

// TestDescribeFindsACRDAddedToAnAlreadyCachedGroupVersion is the describe-path
// counterpart to TestDiscoveryFindsACRDCreatedAfterStartup.
//
// Caching a parsed schema document for the process lifetime would make a CRD
// created later in an already-seen group-version resolvable but not
// describable: the RESTMapper refreshes and finds it, while the cached document
// does not mention it. The property "a new CRD needs no gateway restart" has to
// hold for both paths or it does not hold at all.
func TestDescribeFindsACRDAddedToAnAlreadyCachedGroupVersion(t *testing.T) {
	t.Parallel()
	d, fd := newTestDiscovery(t, "*.example.com")
	ctx := context.Background()

	// Warm the cache for example.com/v1.
	if _, err := d.Describe(ctx, widgetGVK); err != nil {
		t.Fatalf("warm Describe(Widget) = %v", err)
	}
	warmFetches := fd.crdFetches.Load()

	// A second CRD appears in the same group-version, after both the resource
	// list and the schema document were cached.
	gadget := schema.GroupVersionKind{Group: "example.com", Version: "v1", Kind: "Gadget"}
	fd.byGV["example.com/v1"].APIResources = append(fd.byGV["example.com/v1"].APIResources,
		metav1.APIResource{Name: "gadgets", Kind: "Gadget", Namespaced: true,
			Verbs: metav1.Verbs{"get", "list", "watch"}})
	fd.openapi = fakeOpenAPI{calls: fd.pathsCalls, paths: map[string]openapi.GroupVersion{
		"apis/example.com/v1": fakeGroupVersion{fetches: fd.crdFetches,
			doc: openAPIDocFor(t, map[schema.GroupVersionKind][]string{
				widgetGVK: {"apiVersion", "kind", "metadata", "spec", "status"},
				gadget:    {"apiVersion", "kind", "metadata", "spec"},
			})},
	}}
	d.openapi = fd.openapi

	info, err := d.Describe(ctx, gadget)
	if err != nil {
		t.Fatalf("Describe(Gadget) after creation = %v; a CRD added to a cached "+
			"group-version must not need a gateway restart", err)
	}
	if info.Plural != "gadgets" {
		t.Errorf("plural = %q, want gadgets", info.Plural)
	}
	if !strings.Contains(joined(info.Columns), "spec") {
		t.Errorf("columns = %s, want the schema's spec field", joined(info.Columns))
	}
	if fd.crdFetches.Load() <= warmFetches {
		t.Error("the document was never refetched; the result came from a stale cache")
	}

	// The refetched document is cached in turn: describing again is free.
	after := fd.crdFetches.Load()
	if _, err := d.Describe(ctx, gadget); err != nil {
		t.Fatal(err)
	}
	if fd.crdFetches.Load() != after {
		t.Error("a second Describe refetched; the refreshed document was not cached")
	}
}

func TestDiscoveryStats(t *testing.T) {
	t.Parallel()
	d, _ := newTestDiscoveryWithAccess(t, "pods,widgets.example.com", AllowAll{})
	ctx := context.Background()

	if got := d.OpenAPIFetches(); got != 0 {
		t.Errorf("initial OpenAPIFetches = %d, want 0", got)
	}
	if got := d.OpenAPIGroupVersions(); got != 0 {
		t.Errorf("initial OpenAPIGroupVersions = %d, want 0", got)
	}
	if got := d.AccessReviews(); got != 0 {
		t.Errorf("initial AccessReviews = %d, want 0", got)
	}

	// Describe fetches OpenAPI schema for the group version.
	if _, err := d.Describe(ctx, podGVK); err != nil {
		t.Fatal(err)
	}
	if got := d.OpenAPIFetches(); got != 1 {
		t.Errorf("OpenAPIFetches after Describe = %d, want 1", got)
	}
	if got := d.OpenAPIGroupVersions(); got != 1 {
		t.Errorf("OpenAPIGroupVersions after Describe = %d, want 1", got)
	}

	// Repeated Describe uses cache, does not increment fetches or group versions.
	if _, err := d.Describe(ctx, podGVK); err != nil {
		t.Fatal(err)
	}
	if got := d.OpenAPIFetches(); got != 1 {
		t.Errorf("OpenAPIFetches after cached Describe = %d, want 1", got)
	}
	if got := d.OpenAPIGroupVersions(); got != 1 {
		t.Errorf("OpenAPIGroupVersions after cached Describe = %d, want 1", got)
	}
}

// A kind removed from the cluster stops being offered once the cache expires.
//
// Discovery already retries through an invalidation on a miss, so a newly
// created custom resource resolves without a restart. The opposite direction
// had no answer at all: a resource list fetched successfully was never
// refetched, so a deleted kind kept resolving and IMPORT kept generating a
// table for it until the gateway was restarted.
//
// The clock is injected rather than slept through: waiting out the real TTL
// would put five minutes into the suite for one assertion.
func TestDiscoveryStopsOfferingARemovedKind(t *testing.T) {
	t.Parallel()
	d, fd := newTestDiscovery(t, "*.*")
	widget := schema.GroupVersionKind{Group: "example.com", Version: "v1", Kind: "Widget"}

	if _, _, err := d.Resolve(context.Background(), widget); err != nil {
		t.Fatalf("Widget should resolve while the CRD exists: %v", err)
	}

	// The operator is uninstalled: the group-version no longer answers.
	delete(fd.byGV, "example.com/v1")

	// Still cached, so still offered. This is the behaviour being fixed, and
	// asserting it first proves the refetch below is what changes the answer
	// rather than the deletion alone.
	if _, _, err := d.Resolve(context.Background(), widget); err != nil {
		t.Fatalf("within the TTL the cached answer should still be served: %v", err)
	}

	// Past the TTL.
	base := time.Now()
	d.now = func() time.Time { return base }
	d.mu.Lock()
	for gv, e := range d.cache {
		e.fetched = base.Add(-resourceTTL - time.Second)
		d.cache[gv] = e
	}
	d.mu.Unlock()

	_, _, err := d.Resolve(context.Background(), widget)
	if !errors.Is(err, ErrUnsupportedKind) {
		t.Errorf("after the TTL a removed kind must stop resolving, got %v", err)
	}
}
