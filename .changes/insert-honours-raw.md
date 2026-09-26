---
kind: fixed
---

**`INSERT` no longer discards `raw`.** Inserting a whole manifest as `raw`
created an empty object and reported success. Postgres fills every column an
`INSERT` does not mention with NULL, and each NULL cleared the matching field,
so `data`, `labels` and `annotations` from `raw` were wiped. A NULL column now
leaves `raw` alone.

When both are given, a typed column overrides the same field in `raw`, as it
already does on `UPDATE`. That makes `raw` read from one object usable as a
template for another: server-assigned metadata such as `uid` and
`creationTimestamp` is dropped rather than sent. A `raw` whose `apiVersion` or
`kind` names a different kind from the table is now refused. Before, it was
silently relabelled, on `INSERT` and on an `UPDATE` that replaces `raw`.
