package k8s

import (
	"encoding/json"
	"io/fs"
	"os"
	"testing"

	"k8s.io/apimachinery/pkg/runtime/schema"
)

// fixtureFields parses one of the trimmed real documents in testdata/openapi.
func fixtureFields(t *testing.T, file string) map[schema.GroupVersionKind][]Field {
	t.Helper()
	// An fs.FS rooted at the fixtures, so the name cannot reach outside them.
	raw, err := fs.ReadFile(os.DirFS("testdata/openapi"), file)
	if err != nil {
		t.Fatal(err)
	}
	byKind, _, err := fieldsByKind(raw)
	if err != nil {
		t.Fatalf("%s: %v", file, err)
	}
	return byKind
}

func typesOf(fields []Field) map[string]ColumnType {
	out := make(map[string]ColumnType, len(fields))
	for _, f := range fields {
		out[f.Name] = f.Type
	}
	return out
}

// TestColumnTypesFromRealDocuments reads the documents Kubernetes 1.37 serves.
// The expectations are written from the documents by hand, not derived from
// the code under test.
func TestColumnTypesFromRealDocuments(t *testing.T) {
	t.Parallel()
	const (
		txt  = ColumnText
		js   = ColumnJSONB
		i64  = ColumnBigint
		bl   = ColumnBoolean
		tstz = ColumnTimestamptz
	)
	for _, tc := range []struct {
		file string
		gvk  schema.GroupVersionKind
		want map[string]ColumnType
	}{
		{
			// The issue's example: type and reason are strings, count an
			// int32, and the timestamps meta.v1.Time and MicroTime through
			// allOf -- the shape a rule reading only inline types would miss.
			file: "api_v1.json",
			gvk:  schema.GroupVersionKind{Version: "v1", Kind: "Event"},
			want: map[string]ColumnType{
				"action": txt, "apiVersion": txt, "count": i64, "eventTime": tstz,
				"firstTimestamp": tstz, "involvedObject": js, "kind": txt,
				"lastTimestamp": tstz, "message": txt, "metadata": js, "reason": txt,
				"related": js, "reportingComponent": txt, "reportingInstance": txt,
				"series": js, "source": js, "type": txt,
			},
		},
		{
			file: "api_v1.json",
			gvk:  schema.GroupVersionKind{Version: "v1", Kind: "ConfigMap"},
			want: map[string]ColumnType{
				"apiVersion": txt, "binaryData": js, "data": js, "immutable": bl,
				"kind": txt, "metadata": js,
			},
		},
		{
			file: "api_v1.json",
			gvk:  schema.GroupVersionKind{Version: "v1", Kind: "Secret"},
			want: map[string]ColumnType{
				"apiVersion": txt, "data": js, "immutable": bl, "kind": txt,
				"metadata": js, "stringData": js, "type": txt,
			},
		},
		{
			file: "api_v1.json",
			gvk:  schema.GroupVersionKind{Version: "v1", Kind: "Pod"},
			want: map[string]ColumnType{
				"apiVersion": txt, "kind": txt, "metadata": js, "spec": js, "status": js,
			},
		},
		{
			file: "apis_apps_v1.json",
			gvk:  schema.GroupVersionKind{Group: "apps", Version: "v1", Kind: "Deployment"},
			want: map[string]ColumnType{
				"apiVersion": txt, "kind": txt, "metadata": js, "spec": js, "status": js,
			},
		},
		{
			file: "apis_events.k8s.io_v1.json",
			gvk:  schema.GroupVersionKind{Group: "events.k8s.io", Version: "v1", Kind: "Event"},
			want: map[string]ColumnType{
				"action": txt, "apiVersion": txt, "deprecatedCount": i64,
				"deprecatedFirstTimestamp": tstz, "deprecatedLastTimestamp": tstz,
				"deprecatedSource": js, "eventTime": tstz, "kind": txt, "metadata": js,
				"note": txt, "reason": txt, "regarding": js, "related": js,
				"reportingController": txt, "reportingInstance": txt, "series": js,
				"type": txt,
			},
		},
	} {
		t.Run(tc.gvk.String(), func(t *testing.T) {
			t.Parallel()
			fields, ok := fixtureFields(t, tc.file)[tc.gvk]
			if !ok {
				t.Fatalf("%s does not describe %s", tc.file, tc.gvk)
			}
			got := typesOf(fields)
			if len(got) != len(tc.want) {
				t.Errorf("got %d fields, want %d: %v", len(got), len(tc.want), got)
			}
			for name, want := range tc.want {
				if got[name] != want {
					t.Errorf("%s = %v, want %v", name, got[name], want)
				}
			}
		})
	}
}

// TestStaticKindsMatchTheRealDocument holds the static fallback's hand-written
// fields to what discovery reads from the API server, so the two produce the
// same table.
func TestStaticKindsMatchTheRealDocument(t *testing.T) {
	t.Parallel()
	byKind := fixtureFields(t, "api_v1.json")
	for kind, static := range map[string][]Field{"Pod": podTopLevel, "ConfigMap": configMapTopLevel} {
		real := byKind[schema.GroupVersionKind{Version: "v1", Kind: kind}]
		if len(real) != len(static) {
			t.Errorf("%s: static has %d fields, the document %d", kind, len(static), len(real))
			continue
		}
		for i := range real {
			if real[i] != static[i] {
				t.Errorf("%s field %d: static %+v, document %+v", kind, i, static[i], real[i])
			}
		}
	}
}

func TestColumnTypeOf(t *testing.T) {
	t.Parallel()
	// Components as the real documents spell them, plus two that exist only to
	// test resolution.
	components := map[string]string{
		"io.k8s.apimachinery.pkg.apis.meta.v1.Time":       `{"format": "date-time", "type": "string"}`,
		"io.k8s.apimachinery.pkg.util.intstr.IntOrString": `{"format": "int-or-string", "oneOf": [{"type": "integer"}, {"type": "string"}]}`,
		"io.k8s.apimachinery.pkg.api.resource.Quantity":   `{"oneOf": [{"type": "string"}, {"type": "number"}]}`,
		"io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta": `{"type": "object"}`,
		"test.Alias": `{"$ref": "#/components/schemas/io.k8s.apimachinery.pkg.apis.meta.v1.Time"}`,
	}
	resolve := func(name string) (propSchema, bool) {
		raw, ok := components[name]
		if !ok {
			return propSchema{}, false
		}
		var p propSchema
		if err := json.Unmarshal([]byte(raw), &p); err != nil {
			t.Fatalf("%s: %v", name, err)
		}
		return p, true
	}
	ref := func(name string) string {
		return `{"allOf": [{"$ref": "#/components/schemas/` + name + `"}]}`
	}
	for _, tc := range []struct {
		name   string
		schema string
		want   ColumnType
	}{
		{"a string", `{"type": "string"}`, ColumnText},
		{"a base64 string is still text", `{"type": "string", "format": "byte"}`, ColumnText},
		{"an inline date-time, as CRDs spell it", `{"type": "string", "format": "date-time"}`, ColumnTimestamptz},
		{"an int32", `{"type": "integer", "format": "int32"}`, ColumnBigint},
		{"an int64", `{"type": "integer", "format": "int64"}`, ColumnBigint},
		{"a boolean", `{"type": "boolean"}`, ColumnBoolean},
		{"meta.v1.Time through allOf", ref("io.k8s.apimachinery.pkg.apis.meta.v1.Time"), ColumnTimestamptz},
		{"a bare $ref", `{"$ref": "#/components/schemas/io.k8s.apimachinery.pkg.apis.meta.v1.Time"}`, ColumnTimestamptz},
		{"an object through allOf", ref("io.k8s.apimachinery.pkg.apis.meta.v1.ObjectMeta"), ColumnJSONB},

		// Everything below may hold more than one JSON type, or promises
		// nothing, or cannot be resolved: jsonb, which holds any of it.
		{"IntOrString", ref("io.k8s.apimachinery.pkg.util.intstr.IntOrString"), ColumnJSONB},
		{"Quantity", ref("io.k8s.apimachinery.pkg.api.resource.Quantity"), ColumnJSONB},
		{"an inline int-or-string", `{"x-kubernetes-int-or-string": true}`, ColumnJSONB},
		{"an int-or-string that also says string", `{"type": "string", "x-kubernetes-int-or-string": true}`, ColumnJSONB},
		{"a string format int-or-string", `{"type": "string", "format": "int-or-string"}`, ColumnJSONB},
		{"preserve-unknown-fields", `{"type": "string", "x-kubernetes-preserve-unknown-fields": true}`, ColumnJSONB},
		{"anyOf", `{"anyOf": [{"type": "string"}, {"type": "integer"}]}`, ColumnJSONB},
		// A type alongside a oneOf or anyOf does not narrow it: the value may
		// still be any of the alternatives.
		{"a string that may also be an integer", `{"type": "string", "oneOf": [{"type": "string"}, {"type": "integer"}]}`, ColumnJSONB},
		{"an integer that may also be a string", `{"type": "integer", "anyOf": [{"type": "integer"}, {"type": "string"}]}`, ColumnJSONB},
		{"a number, which the extension cannot hold exactly", `{"type": "number", "format": "double"}`, ColumnJSONB},
		{"an object", `{"type": "object"}`, ColumnJSONB},
		{"an array", `{"type": "array", "items": {"type": "string"}}`, ColumnJSONB},
		{"no type at all", `{}`, ColumnJSONB},
		{"OpenAPI 3.1's list of types", `{"type": ["string", "null"]}`, ColumnJSONB},
		{"an allOf of two", `{"allOf": [{"type": "string"}, {"type": "string"}]}`, ColumnJSONB},
		{"an allOf of two beside a type", `{"type": "string", "allOf": [{"minLength": 1}, {"maxLength": 9}]}`, ColumnJSONB},
		{"an allOf that also sets a type", `{"type": "object", "allOf": [{"$ref": "#/components/schemas/io.k8s.apimachinery.pkg.apis.meta.v1.Time"}]}`, ColumnJSONB},
		{"a reference that does not resolve", ref("nowhere"), ColumnJSONB},
		{"a reference outside the document", `{"$ref": "https://example.com/schema.json"}`, ColumnJSONB},
		{"a reference to a reference", ref("test.Alias"), ColumnJSONB},
	} {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()
			var p propSchema
			if err := json.Unmarshal([]byte(tc.schema), &p); err != nil {
				t.Fatal(err)
			}
			if got := columnTypeOf(p, resolve); got != tc.want {
				t.Errorf("columnTypeOf(%s) = %v, want %v", tc.schema, got, tc.want)
			}
		})
	}
}

// TestADocumentWithAnOddTypeStillParses keeps one unexpected spelling from
// failing a whole group-version: every kind in it would vanish from IMPORT.
func TestADocumentWithAnOddTypeStillParses(t *testing.T) {
	t.Parallel()
	doc := `{"components": {"schemas": {"w": {
		"x-kubernetes-group-version-kind": [{"group": "example.com", "version": "v1", "kind": "Widget"}],
		"properties": {"odd": {"type": ["string", "null"]}, "n": {"type": "integer"}}}}}}`
	byKind, _, err := fieldsByKind([]byte(doc))
	if err != nil {
		t.Fatalf("fieldsByKind = %v", err)
	}
	got := typesOf(byKind[schema.GroupVersionKind{Group: "example.com", Version: "v1", Kind: "Widget"}])
	if got["odd"] != ColumnJSONB || got["n"] != ColumnBigint {
		t.Errorf("fields = %v", got)
	}
}
