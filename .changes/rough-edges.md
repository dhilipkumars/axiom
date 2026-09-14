---
kind: fixed
---

Three things found by using Axiom by hand, all operator-facing:

- A kind deleted from the cluster stopped being offered only when the gateway
  was restarted. The cached resource list now expires after five minutes, so a
  re-import reflects the cluster.
- `IMPORT FOREIGN SCHEMA` gets ten times the per-RPC deadline, because it
  enumerates every kind, fetching an OpenAPI document per API group and
  checking access per kind. A whole-cluster import could exceed the ordinary
  timeout outright, and the resulting error named no setting to change. It now
  names one.
- Scanning a table whose kind the gateway no longer serves said only
  "unsupported kind". It now explains that the cause is the cluster, the serve
  list or RBAC, and that the table needs re-importing. It stays an error rather
  than becoming a warning with zero rows, which would be indistinguishable from
  an empty cluster.
