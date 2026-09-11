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
			want:       "name,namespace,uid,resource_version,creation_timestamp,labels,annotations,spec,status,raw",
		},
		{
			name:       "pods keep their hand-mapped phase and node columns",
			gvk:        pod,
			namespaced: true,
			topLevel:   []string{"apiVersion", "kind", "metadata", "spec", "status"},
			want:       "name,namespace,uid,resource_version,creation_timestamp,labels,annotations,phase,node,spec,status,raw",
		},
		{
			name:       "deployments keep their hand-mapped replica columns",
			gvk:        schema.GroupVersionKind{Group: "apps", Version: "v1", Kind: "Deployment"},
			namespaced: true,
			topLevel:   []string{"apiVersion", "kind", "metadata", "spec", "status"},
			want: "name,namespace,uid,resource_version,creation_timestamp,labels,annotations," +
				"replicas,ready_replicas,available_replicas,updated_replicas,spec,status,raw",
		},
		{
			name:       "a Deployment in another group gets no promoted columns",
			gvk:        schema.GroupVersionKind{Group: "example.com", Version: "v1", Kind: "Deployment"},
			namespaced: true,
			topLevel:   []string{"spec", "status"},
			want:       "name,namespace,uid,resource_version,creation_timestamp,labels,annotations,spec,status,raw",
		},
		{
			name:       "a cluster-scoped kind has no namespace column",
			gvk:        widget,
			namespaced: false,
			topLevel:   []string{"spec"},
			want:       "name,uid,resource_version,creation_timestamp,labels,annotations,spec,raw",
		},
		{
			name:       "camelCase top-level fields are normalized",
			gvk:        schema.GroupVersionKind{Version: "v1", Kind: "Secret"},
			namespaced: true,
			topLevel:   []string{"data", "stringData", "type"},
			want:       "name,namespace,uid,resource_version,creation_timestamp,labels,annotations,data,string_data,type,raw",
		},
		{
			name:       "top-level fields are emitted in sorted order regardless of input order",
			gvk:        widget,
			namespaced: true,
			topLevel:   []string{"status", "spec", "alpha"},
			want:       "name,namespace,uid,resource_version,creation_timestamp,labels,annotations,alpha,spec,status,raw",
		},
		{
			name:       "a top-level field colliding with a metadata column is dropped, not renamed",
			gvk:        widget,
			namespaced: true,
			topLevel:   []string{"labels", "spec"},
			want:       "name,namespace,uid,resource_version,creation_timestamp,labels,annotations,spec,raw",
		},
		{
			name:       "two fields normalizing to one name drop both rather than picking a winner",
			gvk:        widget,
			namespaced: true,
			topLevel:   []string{"myField", "my_field", "spec"},
			want:       "name,namespace,uid,resource_version,creation_timestamp,labels,annotations,spec,raw",
		},
		{
			name:       "an over-long field name is dropped rather than truncated into a collision",
			gvk:        widget,
			namespaced: true,
			topLevel:   []string{strings.Repeat("a", maxIdentLen+1), "spec"},
			want:       "name,namespace,uid,resource_version,creation_timestamp,labels,annotations,spec,raw",
		},
		{
			name:       "a kind with no top-level fields still gets metadata and raw",
			gvk:        widget,
			namespaced: true,
			topLevel:   nil,
			want:       "name,uid,resource_version,creation_timestamp,labels,annotations,raw",
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			ns := tc.namespaced
			// The "no top-level fields" case is written cluster-scoped in its
			// expectation; keep the table honest by deriving from want.
			if !strings.Contains(tc.want, "namespace") {
				ns = false
			}
			got := Columns(tc.gvk, ns, tc.topLevel)
			if joined(got) != tc.want {
				t.Errorf("Columns() = %s\n              want %s", joined(got), tc.want)
			}
		})
	}
}

func TestColumnsAlwaysEndsWithRawAndHasNoDuplicates(t *testing.T) {
	t.Parallel()
	got := Columns(schema.GroupVersionKind{Version: "v1", Kind: "Pod"}, true,
		[]string{"spec", "status", "raw", "name", "metadata", "kind", "apiVersion"})
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
			if got[i].Type != ColumnText {
				t.Errorf("%s column %q is not text; the extension renders these as text", gvk, n)
			}
		}
	}
}

func TestColumnTypes(t *testing.T) {
	t.Parallel()
	cols := Columns(schema.GroupVersionKind{Group: "example.com", Version: "v1", Kind: "Widget"}, true,
		[]string{"spec", "status"})
	want := map[string]ColumnType{
		"name": ColumnText, "namespace": ColumnText, "uid": ColumnText,
		"resource_version": ColumnText, "creation_timestamp": ColumnText,
		"labels": ColumnJSONB, "annotations": ColumnJSONB,
		"spec": ColumnJSONB, "status": ColumnJSONB, "raw": ColumnJSONB,
	}
	for _, c := range cols {
		if want[c.Name] != c.Type {
			t.Errorf("column %q type = %v, want %v", c.Name, c.Type, want[c.Name])
		}
		if c.Source == "" {
			t.Errorf("column %q has no Source; generated DDL comments depend on it", c.Name)
		}
	}
	if ColumnText.String() != "text" || ColumnJSONB.String() != "jsonb" {
		t.Error("ColumnType.String must render SQL type names for generated DDL")
	}
}
