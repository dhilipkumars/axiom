Trimmed OpenAPI v3 documents from the Kubernetes source tree, for the column
type tests (#79). They are the real documents an API server serves, reduced so
the tests read a few kilobytes rather than 2 MB:

- each listed kind's schema, whole except for descriptions, with each top-level
  property's nested schema cut to what decides its column type;
- every component a top-level property references, reduced to its own `type`,
  `format` and composition keywords -- which is how `meta.v1.Time` and
  `MicroTime` resolve to `date-time`, and `IntOrString` and `Quantity` to a
  `oneOf`.

Source: `api/openapi-spec/v3/` at kubernetes/kubernetes `v1.37.0`, the version
`gateway/go.mod`'s client libraries track. To refresh, fetch the same files for
a newer tag and trim them the same way; the tests assert on field types, not on
the documents' bytes.
