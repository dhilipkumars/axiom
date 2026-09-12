package k8s

import (
	"context"
	"fmt"
	"sort"
	"strings"

	"k8s.io/apimachinery/pkg/runtime/schema"
)

// StaticMapper is a Mapper over a fixed set of kinds, with no cluster and no
// discovery round-trips.
//
// It is the test double the RPC handlers are unit-tested against
// (docs/RULES.md §2), and it is what a gateway falls back to when discovery is
// unavailable, so the Phase 1-3 kinds keep working on a cluster whose OpenAPI
// endpoint is not reachable. The empty StaticMapper serves nothing.
type StaticMapper struct {
	byGVK map[schema.GroupVersionKind]KindInfo
}

// NewStaticMapper returns a Mapper serving exactly kinds.
func NewStaticMapper(kinds ...KindInfo) *StaticMapper {
	m := &StaticMapper{byGVK: make(map[schema.GroupVersionKind]KindInfo, len(kinds))}
	for _, k := range kinds {
		m.byGVK[k.GVK] = k
	}
	return m
}

// Resolve implements Mapper.
func (m *StaticMapper) Resolve(_ context.Context, gvk schema.GroupVersionKind) (schema.GroupVersionResource, bool, error) {
	k, ok := m.byGVK[gvk]
	if !ok {
		return schema.GroupVersionResource{}, false, fmt.Errorf("%w: %s", ErrUnsupportedKind, gvk.String())
	}
	return gvk.GroupVersion().WithResource(k.Plural), k.Namespaced, nil
}

// Describe implements Mapper.
func (m *StaticMapper) Describe(_ context.Context, gvk schema.GroupVersionKind) (KindInfo, error) {
	k, ok := m.byGVK[gvk]
	if !ok {
		return KindInfo{}, fmt.Errorf("%w: %s", ErrUnsupportedKind, gvk.String())
	}
	return k, nil
}

// Kinds implements Mapper.
func (m *StaticMapper) Kinds(_ context.Context, group *string, plurals []string) ([]KindInfo, error) {
	want := make(map[string]struct{}, len(plurals))
	for _, p := range plurals {
		want[strings.ToLower(p)] = struct{}{}
	}
	out := make([]KindInfo, 0, len(m.byGVK))
	for _, k := range m.byGVK {
		if group != nil && k.GVK.Group != *group {
			continue
		}
		if len(want) > 0 {
			if _, ok := want[strings.ToLower(k.Plural)]; !ok {
				continue
			}
		}
		out = append(out, k)
	}
	sort.Slice(out, func(i, j int) bool {
		if out[i].GVK.Group != out[j].GVK.Group {
			return out[i].GVK.Group < out[j].GVK.Group
		}
		return out[i].Plural < out[j].Plural
	})
	return out, nil
}

// podTopLevel and configMapTopLevel are the top-level schema fields of the two
// built-in kinds, as the API server's OpenAPI document reports them. They are
// spelled out here so the static fallback produces exactly the columns
// discovery would, rather than a subtly different table.
var (
	podTopLevel       = []string{"apiVersion", "kind", "metadata", "spec", "status"}
	configMapTopLevel = []string{"apiVersion", "binaryData", "data", "immutable", "kind", "metadata"}
)

// BuiltinKinds returns the kinds Phases 1-3 served: Pods read-only at the SQL
// layer, ConfigMaps read-write. It seeds the static fallback and the handler
// unit tests so neither depends on a reachable cluster.
func BuiltinKinds() []KindInfo {
	pod := schema.GroupVersionKind{Group: "", Version: "v1", Kind: "Pod"}
	cm := schema.GroupVersionKind{Group: "", Version: "v1", Kind: "ConfigMap"}
	return []KindInfo{
		{
			GVK:        pod,
			Plural:     "pods",
			Namespaced: true,
			Columns:    Columns(pod, true, podTopLevel),
			// Pods are creatable and deletable through the API; the SQL layer
			// declines to expose that (see Kind::writable in the extension),
			// which is a separate decision from what the API supports.
			Writable:  true,
			Watchable: true,
		},
		{
			GVK:        cm,
			Plural:     "configmaps",
			Namespaced: true,
			Columns:    Columns(cm, true, configMapTopLevel),
			Writable:   true,
			Watchable:  true,
		},
	}
}
