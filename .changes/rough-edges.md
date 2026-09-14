---
kind: fixed
---

Three things found by using Axiom by hand, all operator-facing:

- A kind deleted from the cluster stopped being offered only when the gateway
  was restarted. The cached resource list now expires after five minutes, so a
  re-import reflects the cluster.
- An `IMPORT FOREIGN SCHEMA` that ran out of time reported only that a deadline
  expired, without saying which setting governs it. It now names
  `rpc_timeout_secs` and suggests importing one API group at a time. The
  deadline itself is unchanged.
- Scanning a table whose kind the gateway no longer serves said only
  "unsupported kind". It now explains that the cause is the cluster, the serve
  list or RBAC, and that the table needs re-importing. It stays an error rather
  than becoming a warning with zero rows, which would be indistinguishable from
  an empty cluster, and it still names all three causes rather than the actual
  one, so the serve list cannot be enumerated by probing.
