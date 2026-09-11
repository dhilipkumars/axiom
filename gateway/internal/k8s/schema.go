package k8s

import (
	"sort"
	"strings"

	"k8s.io/apimachinery/pkg/runtime/schema"
)

// ColumnType is the Postgres type a discovered column must be declared with.
//
// Deliberately narrow (docs/DESIGN.md §5.4): promoted scalars are text and
// everything structured stays jsonb. An OpenAPI schema often does not constrain
// a field tightly enough to justify a numeric or timestamp column, and guessing
// wrong turns a queryable table into a cast-error minefield.
type ColumnType int

const (
	// ColumnText is SQL `text`.
	ColumnText ColumnType = iota + 1
	// ColumnJSONB is SQL `jsonb`.
	ColumnJSONB
)

// String renders the SQL type name used in generated DDL.
func (c ColumnType) String() string {
	if c == ColumnText {
		return "text"
	}
	return "jsonb"
}

// Column is one column of a kind's foreign table.
type Column struct {
	// Name is the SQL column name, always a bare lowercase identifier.
	Name string
	// Type is the SQL type the column must be declared with.
	Type ColumnType
	// Source names where the value comes from, for diagnostics and DDL
	// comments. It is not a protocol: the extension's projection rule, keyed
	// on Name alone, decides how a column is actually read.
	Source string
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

// metadataColumns are the promoted scalars and maps every kind gets, in the
// order they appear in generated DDL. `namespace` is filtered out for
// cluster-scoped kinds by Columns.
var metadataColumns = []Column{
	{Name: "name", Type: ColumnText, Source: "metadata.name"},
	{Name: "namespace", Type: ColumnText, Source: "metadata.namespace"},
	{Name: "uid", Type: ColumnText, Source: "metadata.uid"},
	{Name: "resource_version", Type: ColumnText, Source: "metadata.resourceVersion"},
	{Name: "creation_timestamp", Type: ColumnText, Source: "metadata.creationTimestamp"},
	{Name: "labels", Type: ColumnJSONB, Source: "metadata.labels"},
	{Name: "annotations", Type: ColumnJSONB, Source: "metadata.annotations"},
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
		{Name: "phase", Type: ColumnText, Source: "status.phase"},
		{Name: "node", Type: ColumnText, Source: "spec.nodeName"},
	},
	// A Deployment's replica counts are what people filter on, and they sit
	// too deep for the generic top-level rule. They are text columns holding a
	// rendered number (Kubernetes models them as JSON numbers), so
	// `replicas::int` works and an absent field is NULL rather than zero.
	{Group: "apps", Version: "v1", Kind: "Deployment"}: {
		{Name: "replicas", Type: ColumnText, Source: "spec.replicas"},
		{Name: "ready_replicas", Type: ColumnText, Source: "status.readyReplicas"},
		{Name: "available_replicas", Type: ColumnText, Source: "status.availableReplicas"},
		{Name: "updated_replicas", Type: ColumnText, Source: "status.updatedReplicas"},
	},
}

// skipTopLevel are the object-identity fields that never become columns of
// their own: apiVersion and kind are fixed by the table's options, and metadata
// is already exploded into the promoted scalars above.
var skipTopLevel = map[string]struct{}{
	"apiVersion": {},
	"kind":       {},
	"metadata":   {},
}

// Columns derives the foreign-table shape of one kind from its identity and the
// top-level field names of its schema.
//
// This is the whole of docs/DESIGN.md §5.4's mapping, kept pure so it is unit
// tested without a cluster (docs/RULES.md §2). Column order is stable: the
// promoted metadata scalars, then any hand-mapped columns for the kind, then
// the kind's own top-level fields in sorted order, then `raw` last.
//
// A top-level field is dropped, rather than renamed or truncated, when its
// normalized name collides with a column already emitted, exceeds a Postgres
// identifier's length, or normalizes to the same name as another top-level
// field. Dropping loses nothing: every field remains reachable through `raw`,
// whereas a silently renamed column would be a column whose value the extension
// could not find (docs/RULES.md §1, no silent degrade).
func Columns(gvk schema.GroupVersionKind, namespaced bool, topLevel []string) []Column {
	cols := make([]Column, 0, len(metadataColumns)+len(topLevel)+3)
	taken := make(map[string]struct{}, len(metadataColumns)+len(topLevel)+3)
	add := func(c Column) bool {
		if _, dup := taken[c.Name]; dup {
			return false
		}
		taken[c.Name] = struct{}{}
		cols = append(cols, c)
		return true
	}

	for _, c := range metadataColumns {
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
		if _, skip := skipTopLevel[f]; skip {
			continue
		}
		counts[NormalizeFieldName(f)]++
	}

	fields := append([]string(nil), topLevel...)
	sort.Strings(fields)
	for _, f := range fields {
		if _, skip := skipTopLevel[f]; skip {
			continue
		}
		name := NormalizeFieldName(f)
		if name == "" || len(name) > maxIdentLen || counts[name] > 1 {
			continue
		}
		add(Column{Name: name, Type: ColumnJSONB, Source: f})
	}

	// `raw` is last and always present: it is the escape hatch for everything
	// the rules above dropped, and the carrier of identity and resourceVersion
	// for the write path.
	cols = append(cols, Column{Name: "raw", Type: ColumnJSONB, Source: "the whole object"})
	return cols
}
