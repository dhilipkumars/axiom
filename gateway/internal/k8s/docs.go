package k8s

import (
	"sort"

	"k8s.io/apimachinery/pkg/runtime/schema"
)

// This file exposes the column tables for documentation generation
// (docs/PLAN.md Phase 6 Part 2). The reference page in docs/generated is
// produced by walking these, not by transcribing them, so a column added or
// removed here changes the page and CI fails on the uncommitted diff.
//
// They are accessors rather than exported variables so a caller cannot mutate
// the tables the server is using. The returned slices are copies.

// UniversalColumns returns the columns every kind gets regardless of its
// schema, in generated-DDL order.
func UniversalColumns() []Column {
	out := make([]Column, len(universalColumns))
	copy(out, universalColumns)
	return out
}

// PromotedKinds returns the kinds that carry hand-mapped columns, in a stable
// order (group, version, kind).
func PromotedKinds() []schema.GroupVersionKind {
	out := make([]schema.GroupVersionKind, 0, len(promoted))
	for gvk := range promoted {
		out = append(out, gvk)
	}
	sort.Slice(out, func(i, j int) bool {
		a, b := out[i], out[j]
		if a.Group != b.Group {
			return a.Group < b.Group
		}
		if a.Version != b.Version {
			return a.Version < b.Version
		}
		return a.Kind < b.Kind
	})
	return out
}

// PromotedColumns returns the hand-mapped columns for one kind.
func PromotedColumns(gvk schema.GroupVersionKind) []Column {
	out := make([]Column, len(promoted[gvk]))
	copy(out, promoted[gvk])
	return out
}

// SkippedTopLevelFields returns the object fields the generic top-level rule
// does not emit a column for, because a universal column already covers them.
func SkippedTopLevelFields() []string {
	out := make([]string, 0, len(skipTopLevel))
	for f := range skipTopLevel {
		out = append(out, f)
	}
	sort.Strings(out)
	return out
}
