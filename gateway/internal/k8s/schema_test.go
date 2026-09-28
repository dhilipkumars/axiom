package k8s

import (
	"strings"
	"testing"

	"k8s.io/apimachinery/pkg/runtime/schema"
)

func TestNormalizeFieldName(t *testing.T) {
	t.Parallel()
	// These cases are mirrored by normalize_field_name's tests in
	// extension/src/schema.rs. The two implementations are a contract: a
	// change here that is not made there turns a column silently NULL.
	for _, tc := range []struct{ in, want string }{
		{"spec", "spec"},
		{"status", "status"},
		{"data", "data"},
		{"stringData", "string_data"},
		{"roleRef", "role_ref"},
		{"imagePullSecrets", "image_pull_secrets"},
		{"AllCaps", "all_caps"},
		{"with-dash", "with_dash"},
		{"with.dot", "with_dot"},
		{"with space", "with_space"},
		{"already_snake", "already_snake"},
		{"x509", "x509"},
		{"3rdParty", "_3rd_party"},
		{"", ""},
	} {
		if got := NormalizeFieldName(tc.in); got != tc.want {
			t.Errorf("NormalizeFieldName(%q) = %q, want %q", tc.in, got, tc.want)
		}
	}
}

func TestNormalizeFieldNameNeverProducesAnInvalidIdentifierStart(t *testing.T) {
	t.Parallel()
	for _, in := range []string{"9lives", "0", "-x", ".y", " z"} {
		got := NormalizeFieldName(in)
		if got == "" {
			t.Fatalf("NormalizeFieldName(%q) = empty", in)
		}
		c := got[0]
		if c >= '0' && c <= '9' {
			t.Errorf("NormalizeFieldName(%q) = %q, which starts with a digit", in, got)
		}
	}
}

// names extracts column names in order, for compact assertions.
func names(cols []Column) []string {
	out := make([]string, len(cols))
	for i, c := range cols {
		out[i] = c.Name
	}
	return out
}

func joined(cols []Column) string { return strings.Join(names(cols), ",") }

func TestColumns(t *testing.T) {
	t.Parallel()
	pod := schema.GroupVersionKind{Group: "", Version: "v1", Kind: "Pod"}
	widget := schema.GroupVersionKind{Group: "example.com", Version: "v1", Kind: "Widget"}

	for _, tc := range []struct {
		name       string
		gvk        schema.GroupVersionKind
		namespaced bool
		topLevel   []string
		want       string
	}{
		{
			name:       "a CRD gets promoted metadata, its own top-level fields, and raw",
			gvk:        widget,
			namespaced: true,
			topLevel:   []string{"apiVersion", "kind", "metadata", "spec", "status"},
			want:       "api_version,kind,name,namespace,uid,resource_version,creation_timestamp,labels,annotations,metadata,spec,status,raw",
		},
		{
			name:       "pods keep their hand-mapped phase and node columns",
			gvk:        pod,
			namespaced: true,
			topLevel:   []string{"apiVersion", "kind", "metadata", "spec", "status"},
			want:       "api_version,kind,name,namespace,uid,resource_version,creation_timestamp,labels,annotations,metadata,phase,node,spec,status,raw",
		},
		{
			name:       "deployments keep their hand-mapped replica columns",
			gvk:        schema.GroupVersionKind{Group: "apps", Version: "v1", Kind: "Deployment"},
			namespaced: true,
			topLevel:   []string{"apiVersion", "kind", "metadata", "spec", "status"},
			want: "api_version,kind,name,namespace,uid,resource_version,creation_timestamp,labels,annotations,metadata," +
				"replicas,ready_replicas,available_replicas,updated_replicas,spec,status,raw",
		},
		{
			name:       "a Deployment in another group gets no promoted columns",
			gvk:        schema.GroupVersionKind{Group: "example.com", Version: "v1", Kind: "Deployment"},
			namespaced: true,
			topLevel:   []string{"spec", "status"},
			want:       "api_version,kind,name,namespace,uid,resource_version,creation_timestamp,labels,annotations,metadata,spec,status,raw",
		},
		{
			name:       "a cluster-scoped kind has no namespace column",
			gvk:        widget,
			namespaced: false,
			topLevel:   []string{"spec"},
			want:       "api_version,kind,name,uid,resource_version,creation_timestamp,labels,annotations,metadata,spec,raw",
		},
		{
			name:       "camelCase top-level fields are normalized",
			gvk:        schema.GroupVersionKind{Version: "v1", Kind: "Secret"},
			namespaced: true,
			topLevel:   []string{"data", "stringData", "type"},
			want:       "api_version,kind,name,namespace,uid,resource_version,creation_timestamp,labels,annotations,metadata,data,string_data,type,raw",
		},
		{
			name:       "top-level fields are emitted in sorted order regardless of input order",
			gvk:        widget,
			namespaced: true,
			topLevel:   []string{"status", "spec", "alpha"},
			want:       "api_version,kind,name,namespace,uid,resource_version,creation_timestamp,labels,annotations,metadata,alpha,spec,status,raw",
		},
		{
			name:       "a top-level field colliding with a metadata column is dropped, not renamed",
			gvk:        widget,
			namespaced: true,
			topLevel:   []string{"labels", "spec"},
			want:       "api_version,kind,name,namespace,uid,resource_version,creation_timestamp,labels,annotations,metadata,spec,raw",
		},
		{
			name:       "two fields normalizing to one name drop both rather than picking a winner",
			gvk:        widget,
			namespaced: true,
			topLevel:   []string{"myField", "my_field", "spec"},
			want:       "api_version,kind,name,namespace,uid,resource_version,creation_timestamp,labels,annotations,metadata,spec,raw",
		},
		{
			name:       "an over-long field name is dropped rather than truncated into a collision",
			gvk:        widget,
			namespaced: true,
			topLevel:   []string{strings.Repeat("a", maxIdentLen+1), "spec"},
			want:       "api_version,kind,name,namespace,uid,resource_version,creation_timestamp,labels,annotations,metadata,spec,raw",
		},
		{
			name:       "a kind with no top-level fields still gets metadata and raw",
			gvk:        widget,
			namespaced: true,
			topLevel:   nil,
			want:       "api_version,kind,name,uid,resource_version,creation_timestamp,labels,annotations,metadata,raw",
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			ns := tc.namespaced
			// The "no top-level fields" case is written cluster-scoped in its
			// expectation; keep the table honest by deriving from want.
			if !strings.Contains(tc.want, ",namespace,") {
				ns = false
			}
			got := Columns(tc.gvk, ns, objectFields(tc.topLevel...))
			if joined(got) != tc.want {
				t.Errorf("Columns() = %s\n              want %s", joined(got), tc.want)
			}
		})
	}
}

func TestColumnsAlwaysEndsWithRawAndHasNoDuplicates(t *testing.T) {
	t.Parallel()
	got := Columns(schema.GroupVersionKind{Version: "v1", Kind: "Pod"}, true,
		objectFields("spec", "status", "raw", "name", "metadata", "kind", "apiVersion"))
	if got[len(got)-1].Name != "raw" {
		t.Errorf("last column = %q, want raw", got[len(got)-1].Name)
	}
	seen := map[string]bool{}
	for _, c := range got {
		if seen[c.Name] {
			t.Errorf("duplicate column %q in %s", c.Name, joined(got))
		}
		seen[c.Name] = true
	}
}

// TestPromotedColumnsMatchTheExtension pins the exact promoted set per kind.
// The extension holds the same table keyed by the same GVKs (promoted_columns
// in extension/src/schema.rs) and has the mirror of this test. A column added
// on one side only reads as NULL rather than failing, which is why both sides
// assert the set rather than relying on review.
func TestPromotedColumnsMatchTheExtension(t *testing.T) {
	t.Parallel()
	want := map[schema.GroupVersionKind][]string{
		{Group: "", Version: "v1", Kind: "Pod"}: {"phase", "node"},
		{Group: "apps", Version: "v1", Kind: "Deployment"}: {
			"replicas", "ready_replicas", "available_replicas", "updated_replicas",
		},
	}
	// The extension accepts these types for these columns (Column::accepts in
	// extension/src/schema.rs), and text for each, which is what a caller
	// that predates typed columns is sent.
	wantType := map[string]ColumnType{
		"phase": ColumnText, "node": ColumnText,
		"replicas": ColumnBigint, "ready_replicas": ColumnBigint,
		"available_replicas": ColumnBigint, "updated_replicas": ColumnBigint,
	}
	if len(promoted) != len(want) {
		t.Fatalf("promoted has %d kinds, want %d: update the extension's table too", len(promoted), len(want))
	}
	for gvk, names := range want {
		got := promoted[gvk]
		if len(got) != len(names) {
			t.Errorf("%s: got %d columns, want %d", gvk, len(got), len(names))
			continue
		}
		for i, n := range names {
			if got[i].Name != n {
				t.Errorf("%s column %d = %q, want %q", gvk, i, got[i].Name, n)
			}
			if got[i].Type != wantType[n] {
				t.Errorf("%s column %q is %v, want %v", gvk, n, got[i].Type, wantType[n])
			}
			if got[i].Untyped != ColumnText {
				t.Errorf("%s column %q untyped is %v; before #79 it was text", gvk, n, got[i].Untyped)
			}
		}
	}
}

func TestColumnTypes(t *testing.T) {
	t.Parallel()
	cols := Columns(schema.GroupVersionKind{Group: "example.com", Version: "v1", Kind: "Widget"}, true,
		[]Field{
			{Name: "spec", Type: ColumnJSONB},
			{Name: "status", Type: ColumnJSONB},
			{Name: "count", Type: ColumnBigint},
			{Name: "enabled", Type: ColumnBoolean},
			{Name: "seenAt", Type: ColumnTimestamptz},
			{Name: "note", Type: ColumnText},
			// A caller that did not say leaves it zero: jsonb holds anything.
			{Name: "unknown"},
		})
	// Type, and what a caller that predates typed columns gets instead: its
	// type before #79, text for a metadata scalar and jsonb for a top-level
	// field.
	want := map[string][2]ColumnType{
		"api_version": {ColumnText, ColumnText}, "kind": {ColumnText, ColumnText},
		"metadata": {ColumnJSONB, ColumnJSONB},
		"name":     {ColumnText, ColumnText}, "namespace": {ColumnText, ColumnText},
		"uid": {ColumnText, ColumnText}, "resource_version": {ColumnText, ColumnText},
		"creation_timestamp": {ColumnTimestamptz, ColumnText},
		"labels":             {ColumnJSONB, ColumnJSONB}, "annotations": {ColumnJSONB, ColumnJSONB},
		"spec": {ColumnJSONB, ColumnJSONB}, "status": {ColumnJSONB, ColumnJSONB},
		"count": {ColumnBigint, ColumnJSONB}, "enabled": {ColumnBoolean, ColumnJSONB},
		"seen_at": {ColumnTimestamptz, ColumnJSONB}, "note": {ColumnText, ColumnJSONB},
		"unknown": {ColumnJSONB, ColumnJSONB},
		"raw":     {ColumnJSONB, ColumnJSONB},
	}
	if len(cols) != len(want) {
		t.Errorf("got %d columns (%s), want %d", len(cols), joined(cols), len(want))
	}
	for _, c := range cols {
		w, ok := want[c.Name]
		if !ok {
			t.Errorf("unexpected column %q", c.Name)
			continue
		}
		if c.TypeFor(true) != w[0] || c.TypeFor(false) != w[1] {
			t.Errorf("column %q types = %v/%v, want %v/%v", c.Name, c.TypeFor(true), c.TypeFor(false), w[0], w[1])
		}
		if c.Source == "" {
			t.Errorf("column %q has no Source; generated DDL comments depend on it", c.Name)
		}
	}
}

func TestColumnTypeNamesAreTheDDLSpellings(t *testing.T) {
	t.Parallel()
	for ct, want := range map[ColumnType]string{
		ColumnText: "text", ColumnJSONB: "jsonb", ColumnBigint: "bigint",
		ColumnBoolean: "boolean", ColumnTimestamptz: "timestamptz",
	} {
		if got := ct.String(); got != want {
			t.Errorf("%d.String() = %q, want %q", int(ct), got, want)
		}
	}
	// An unnamed type must not quietly render as a real one.
	if got := ColumnType(99).String(); got != "ColumnType(99)" {
		t.Errorf("an unknown type renders as %q", got)
	}
}

// objectFields is top-level fields as a schema of objects would give them:
// every one jsonb.
func objectFields(names ...string) []Field {
	out := make([]Field, 0, len(names))
	for _, n := range names {
		out = append(out, Field{Name: n, Type: ColumnJSONB})
	}
	return out
}
