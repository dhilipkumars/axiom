---
kind: added
---

**Columns read as the type you declare them.** A foreign table column can now
be `bigint`, `boolean` or `timestamptz` as well as `text` and `jsonb`, and then
compares and sorts as that type, with no cast. `ALTER FOREIGN TABLE
k8s.core_events ALTER COLUMN count TYPE bigint` makes `ORDER BY count DESC`
numeric, and `creation_timestamp` can be `timestamptz`, so
`WHERE creation_timestamp < now() - interval '7 days'` works as written.

A value that is not of the declared type reads as NULL. Writes produce JSON of
the same type: `SET count = 5` writes the number `5`, not the string `"5"`.

Existing tables are unaffected: every column keeps its current type and reads
as it did.
