---
kind: fixed
---

**An INSERT writes a `camelCase` field under its real name.** Setting a column
such as `string_data` on an imported table wrote `string_data` into the new
object instead of `stringData`, and the API server dropped it without an error:
the Secret was created empty. It affected every top-level field Kubernetes
spells in `camelCase`, including `stringData`, `binaryData` and a CRD's own.

`IMPORT FOREIGN SCHEMA` now records each such field's spelling on its column,
as `string_data jsonb OPTIONS (field 'stringData')`. A column's `OPTIONS` are
now validated too, so a misspelt option is an error rather than ignored.

**Tables imported before this still write the old name.** Import them again,
or add the option to the columns you write:

```sql
ALTER FOREIGN TABLE k8s.core_secrets
  ALTER COLUMN string_data OPTIONS (ADD field 'stringData');
```

Hand-written tables without the option behave as before. Any gateway version
works: it already sent each field's spelling.
