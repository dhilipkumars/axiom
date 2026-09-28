package k8s

import (
	"encoding/json"
	"strings"
)

// propSchema is the part of an OpenAPI v3 schema object that decides what
// column type a field gets.
//
// type is kept raw because OpenAPI 3.1 allows a list of types there, and one
// unexpected spelling must not fail the whole document's decode: a field whose
// type is not a single string is jsonb.
type propSchema struct {
	Type            json.RawMessage   `json:"type"`
	Format          string            `json:"format"`
	Ref             string            `json:"$ref"`
	AllOf           []propSchema      `json:"allOf"`
	AnyOf           []json.RawMessage `json:"anyOf"`
	OneOf           []json.RawMessage `json:"oneOf"`
	IntOrString     bool              `json:"x-kubernetes-int-or-string"`
	PreserveUnknown bool              `json:"x-kubernetes-preserve-unknown-fields"`
}

// componentRefPrefix is how a document refers to one of its own components.
const componentRefPrefix = "#/components/schemas/"

// columnTypeOf is the column type a top-level field's schema supports (#79).
//
// A field is typed only when its schema names exactly one scalar type, inline
// or through one reference -- which is how Kubernetes spells a timestamp:
// `allOf: [{$ref: ...meta.v1.Time}]`, whose component is a `date-time`
// string. Everything else is jsonb:
//
//   - objects and arrays, which is most of spec and status;
//   - anything that may be one of several types: `oneOf`/`anyOf` (IntOrString
//     and Quantity are both spelled this way), `x-kubernetes-int-or-string`;
//   - `x-kubernetes-preserve-unknown-fields`, which promises nothing;
//   - `number`: the extension parses JSON without arbitrary precision, so a
//     numeric column would only look exact;
//   - a reference that does not resolve, or resolves to another reference.
//
// jsonb is the safe answer because it holds any value. A wrongly typed column
// would read NULL for the values that do not fit, and would write back a
// different JSON type than the field holds.
func columnTypeOf(p propSchema, resolve func(name string) (propSchema, bool)) ColumnType {
	return fieldType(p, resolve, 0)
}

func fieldType(p propSchema, resolve func(name string) (propSchema, bool), depth int) ColumnType {
	if p.IntOrString || p.PreserveUnknown || len(p.AnyOf) > 0 || len(p.OneOf) > 0 {
		return ColumnJSONB
	}
	switch {
	case len(p.AllOf) > 1:
		return ColumnJSONB
	case len(p.AllOf) == 1:
		if len(p.Type) > 0 || p.Ref != "" {
			// A composition that also constrains the field itself is not the
			// plain one-reference wrapper Kubernetes emits.
			return ColumnJSONB
		}
		return fieldType(p.AllOf[0], resolve, depth)
	case p.Ref != "":
		name, ok := strings.CutPrefix(p.Ref, componentRefPrefix)
		if !ok || depth > 0 {
			return ColumnJSONB
		}
		target, ok := resolve(name)
		if !ok {
			return ColumnJSONB
		}
		return fieldType(target, resolve, depth+1)
	}

	var t string
	if err := json.Unmarshal(p.Type, &t); err != nil {
		return ColumnJSONB
	}
	switch t {
	case "string":
		switch p.Format {
		case "date-time":
			return ColumnTimestamptz
		case "int-or-string":
			return ColumnJSONB
		}
		return ColumnText
	case "integer":
		return ColumnBigint
	case "boolean":
		return ColumnBoolean
	}
	return ColumnJSONB
}
