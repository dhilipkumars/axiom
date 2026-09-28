---
kind: changed
---

**`IMPORT FOREIGN SCHEMA` gives columns the type the kind's schema declares.**
A field Kubernetes declares as a string, integer, boolean or timestamp is now
imported as `text`, `bigint`, `boolean` or `timestamptz`, so the query you
would naturally write works:

```sql
SELECT namespace, name, reason, count FROM k8s.core_events
 WHERE type = 'Warning' ORDER BY count DESC;
```

`creation_timestamp` is `timestamptz` and a Deployment's replica counts are
`bigint`, so `ORDER BY replicas DESC` puts 10 above 9. Objects, arrays and
fields that may hold more than one type, such as quantities, stay `jsonb`.

**Re-importing changes column types.** Tables imported before this keep their
types and keep working. After a re-import, a query written for the old types
fails with an error rather than answering differently: `type->>0` on what is
now a `text` column is `operator does not exist`; write `type = 'Warning'`.
Casts such as `replicas::int` still work.

The gateway and extension must both be this version or later for typed
imports; either one alone keeps importing `text` and `jsonb` as before.
