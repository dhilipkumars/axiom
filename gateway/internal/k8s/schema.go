package k8s

import (
	"fmt"
	"sort"
	"strings"

	"k8s.io/apimachinery/pkg/runtime/schema"
)

// ColumnType is the Postgres type a discovered column is declared with.
//
// A field gets a scalar type only when its OpenAPI schema names exactly one
// (see columnTypeOf); anything structured or ambiguous is jsonb (docs/DESIGN.md
// §5.4, #79). The extension converts each value to and from the declared
// type, and reads a value that is not of that type as NULL.
type ColumnType int

const (
	// ColumnText is SQL `text`.
	ColumnText ColumnType = iota + 1
	// ColumnJSONB is SQL `jsonb`.
	ColumnJSONB
	// ColumnBigint is SQL `bigint`.
	ColumnBigint
	// ColumnBoolean is SQL `boolean`.
	ColumnBoolean
	// ColumnTimestamptz is SQL `timestamptz`.
	ColumnTimestamptz
)

// String renders the SQL type name used in generated DDL.
func (c ColumnType) String() string {
	switch c {
	case ColumnText:
		return "text"
	case ColumnJSONB:
		return "jsonb"
	case ColumnBigint:
		return "bigint"
	case ColumnBoolean:
		return "boolean"
	case ColumnTimestamptz:
		return "timestamptz"
	}
	// Not "jsonb": a type added here without a name must not quietly become
	// a different type in someone's DDL.
	return fmt.Sprintf("ColumnType(%d)", int(c))
}

// Column is one column of a kind's foreign table.
type Column struct {
	// Name is the SQL column name, always a bare lowercase identifier.
	Name string
	// Type is the SQL type the column is declared with.
	Type ColumnType
	// Untyped is the type sent instead to a caller that predates bigint,
	// boolean and timestamptz columns: what the column was before #79, text
	// for a promoted scalar and jsonb for a top-level field. Such an extension
	// drops a column whose type it does not know from the table it generates.
	Untyped ColumnType
	// Source names where the value comes from, for diagnostics and DDL
	// comments. It is not a protocol: the extension's projection rule, keyed
	// on Name alone, decides how a column is actually read.
	Source string
}

// TypeFor is the column's type for a caller that does, or does not, accept
// typed columns.
func (c Column) TypeFor(typed bool) ColumnType {
	if typed {
		return c.Type
	}
	return c.Untyped
}

// Field is one top-level property of a kind's schema, with the column type
// its schema supports.
type Field struct {
	Name string
	Type ColumnType
}

// KindInfo is the discovered shape of one kind: enough for the extension to
// emit a `CREATE FOREIGN TABLE` and then serve it without further discovery.
type KindInfo struct {
	GVK        schema.GroupVersionKind
	Plural     string
	Namespaced bool
	Columns    []Column
	// Writable reports that the API server advertises create, update and
	// delete for the kind. It describes the API, not this gateway's RBAC: a
	// write may still be denied when it executes.
	Writable bool
	// Watchable reports that the API server advertises the watch verb, so a
	// `cache_mode 'watch'` foreign table is possible.
	Watchable bool
}

// NormalizeFieldName maps a Kubernetes field name to the SQL column name that
// represents it.
//
// This function is half of a contract: the extension implements the same rule
// (see `normalize_field_name` in extension/src/schema.rs) and uses it in the
// other direction, matching a declared column back to a top-level field of the
// object at scan time. Neither side ships a field-to-column table, so the two
// cannot drift apart on a per-kind basis; they can only drift if this rule
// itself is changed on one side, which the matching unit tests on both sides
// are there to catch.
//
// The mapping is camelCase to snake_case: an underscore is inserted at each
// case boundary, every character outside [a-z0-9_] becomes an underscore, runs
// of underscores collapse, and a leading digit is prefixed with an underscore
// so the result is always a valid unquoted identifier start. Acronym runs stay
// together, so "APIVersion" becomes "api_version" rather than "a_p_i_version".
//
// It never truncates: a name too long for a Postgres identifier is dropped by
// the caller rather than silently shortened into a collision.
func NormalizeFieldName(field string) string {
	if field == "" {
		return ""
	}
	runes := []rune(field)
	isUpper := func(r rune) bool { return r >= 'A' && r <= 'Z' }
	isLower := func(r rune) bool { return r >= 'a' && r <= 'z' }
	isDigit := func(r rune) bool { return r >= '0' && r <= '9' }

	var b strings.Builder
	b.Grow(len(field) + 4)
	writeSep := func() {
		// Collapse runs: never emit a separator at the start or after another.
		if b.Len() > 0 && b.String()[b.Len()-1] != '_' {
			b.WriteByte('_')
		}
	}
	for i, r := range runes {
		switch {
		case isUpper(r):
			// A boundary is a lower/digit-to-upper transition, or the last
			// upper of an acronym run that is followed by a lowercase letter.
			if i > 0 {
				prev := runes[i-1]
				endOfAcronym := isUpper(prev) && i+1 < len(runes) && isLower(runes[i+1])
				if isLower(prev) || isDigit(prev) || endOfAcronym {
					writeSep()
				}
			}
			b.WriteRune(r + ('a' - 'A'))
		case isLower(r), isDigit(r):
			b.WriteRune(r)
		default:
			writeSep()
		}
	}
	out := strings.TrimRight(b.String(), "_")
	if out == "" {
		return ""
	}
	if isDigit(rune(out[0])) {
		return "_" + out
	}
	return out
}

// maxIdentLen is Postgres's NAMEDATALEN-1. A generated column name longer than
// this would be silently truncated by the server, which can collide two
// distinct fields onto one column, so such a field is dropped instead.
const maxIdentLen = 63

// universalColumns are the columns every kind gets regardless of its schema,
// in the order they appear in generated DDL. The order mirrors a Kubernetes
// object's own: the type identity, then metadata exploded into the scalars
// people filter on, then metadata whole.
//
// `api_version`, `kind` and `metadata` are the only three fields guaranteed to
// exist on every object -- `spec` is present on about two thirds of built-in
// kinds and `status` on under half -- so they are what a query spanning kinds
// has to key on. `metadata` also carries fields not promoted individually,
// such as ownerReferences, finalizers and deletionTimestamp.
//
// `namespace` is filtered out for cluster-scoped kinds by Columns.
var universalColumns = []Column{
	{Name: "api_version", Type: ColumnText, Untyped: ColumnText, Source: "apiVersion"},
	{Name: "kind", Type: ColumnText, Untyped: ColumnText, Source: "kind"},
	{Name: "name", Type: ColumnText, Untyped: ColumnText, Source: "metadata.name"},
	{Name: "namespace", Type: ColumnText, Untyped: ColumnText, Source: "metadata.namespace"},
	{Name: "uid", Type: ColumnText, Untyped: ColumnText, Source: "metadata.uid"},
	{Name: "resource_version", Type: ColumnText, Untyped: ColumnText, Source: "metadata.resourceVersion"},
	{Name: "creation_timestamp", Type: ColumnTimestamptz, Untyped: ColumnText, Source: "metadata.creationTimestamp"},
	{Name: "labels", Type: ColumnJSONB, Untyped: ColumnJSONB, Source: "metadata.labels"},
	{Name: "annotations", Type: ColumnJSONB, Untyped: ColumnJSONB, Source: "metadata.annotations"},
	{Name: "metadata", Type: ColumnJSONB, Untyped: ColumnJSONB, Source: "metadata"},
}

// promoted holds the hand-mapped columns for built-in kinds whose useful
// fields are nested too deep for the generic top-level rule to reach
// (docs/DESIGN.md §5.4, "hand-mapped typed columns for the fields that
// matter").
//
// This table is the second half of a contract with the extension, which holds
// the same GVK-keyed overrides and the JSON pointers to read them from (see
// `promoted_columns` in extension/src/schema.rs). Keep the two in step: a
// column listed here but not there reads as NULL, and the unit tests on both
// sides assert the exact set.
var promoted = map[schema.GroupVersionKind][]Column{
	{Group: "", Version: "v1", Kind: "Pod"}: {
		{Name: "phase", Type: ColumnText, Untyped: ColumnText, Source: "status.phase"},
		{Name: "node", Type: ColumnText, Untyped: ColumnText, Source: "spec.nodeName"},
	},
	// A Deployment's replica counts are what people filter on, and they sit
	// too deep for the generic top-level rule. Kubernetes models them as
	// integers, so they are bigint and order as numbers; an absent field is
	// NULL rather than zero.
	{Group: "apps", Version: "v1", Kind: "Deployment"}: {
		{Name: "replicas", Type: ColumnBigint, Untyped: ColumnText, Source: "spec.replicas"},
		{Name: "ready_replicas", Type: ColumnBigint, Untyped: ColumnText, Source: "status.readyReplicas"},
		{Name: "available_replicas", Type: ColumnBigint, Untyped: ColumnText, Source: "status.availableReplicas"},
		{Name: "updated_replicas", Type: ColumnBigint, Untyped: ColumnText, Source: "status.updatedReplicas"},
	},
}

// skipTopLevel are the fields already covered by universalColumns, so the
// generic top-level rule must not emit a second column for them.
var skipTopLevel = map[string]struct{}{
	"apiVersion": {},
	"kind":       {},
	"metadata":   {},
}

// Columns derives the foreign-table shape of one kind from its identity and the
// top-level fields of its schema.
//
// This is the whole of docs/DESIGN.md §5.4's mapping, kept pure so it is unit
// tested without a cluster (docs/RULES.md §2). Column order is stable: the
// promoted metadata scalars, then any hand-mapped columns for the kind, then
// the kind's own top-level fields in sorted order, then `raw` last.
//
// Every kind gets `api_version`, `kind` and `metadata` whatever its schema:
// they are the only fields present on every object, and therefore the only
// basis for a query spanning kinds.
//
// A top-level field is dropped, rather than renamed or truncated, when its
// normalized name collides with a column already emitted, exceeds a Postgres
// identifier's length, or normalizes to the same name as another top-level
// field. Dropping loses nothing: every field remains reachable through `raw`,
// whereas a silently renamed column would be a column whose value the extension
// could not find (docs/RULES.md §1, no silent degrade).
func Columns(gvk schema.GroupVersionKind, namespaced bool, topLevel []Field) []Column {
	cols := make([]Column, 0, len(universalColumns)+len(topLevel)+3)
	taken := make(map[string]struct{}, len(universalColumns)+len(topLevel)+3)
	add := func(c Column) bool {
		if _, dup := taken[c.Name]; dup {
			return false
		}
		taken[c.Name] = struct{}{}
		cols = append(cols, c)
		return true
	}

	for _, c := range universalColumns {
		if c.Name == "namespace" && !namespaced {
			continue
		}
		add(c)
	}
	for _, c := range promoted[gvk] {
		add(c)
	}
	// Reserve `raw` before the top-level fields are considered, so a kind that
	// happens to have a top-level field named "raw" loses that column rather
	// than displacing the escape hatch every other dropped field depends on.
	taken["raw"] = struct{}{}

	// Count normalizations first so an ambiguous pair (two fields mapping to
	// one column) drops both rather than letting sort order pick a winner.
	counts := make(map[string]int, len(topLevel))
	for _, f := range topLevel {
		if _, skip := skipTopLevel[f.Name]; skip {
			continue
		}
		counts[NormalizeFieldName(f.Name)]++
	}

	fields := append([]Field(nil), topLevel...)
	sort.Slice(fields, func(i, j int) bool { return fields[i].Name < fields[j].Name })
	for _, f := range fields {
		if _, skip := skipTopLevel[f.Name]; skip {
			continue
		}
		name := NormalizeFieldName(f.Name)
		if name == "" || len(name) > maxIdentLen || counts[name] > 1 {
			continue
		}
		t := f.Type
		if t == 0 {
			t = ColumnJSONB
		}
		add(Column{Name: name, Type: t, Untyped: ColumnJSONB, Source: f.Name})
	}

	// `raw` is last and always present: it is the escape hatch for everything
	// the rules above dropped, and the carrier of identity and resourceVersion
	// for the write path.
	cols = append(cols, Column{Name: "raw", Type: ColumnJSONB, Untyped: ColumnJSONB, Source: "the whole object"})
	return cols
}
