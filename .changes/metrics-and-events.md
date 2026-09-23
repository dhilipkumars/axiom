---
kind: added
---

**Usage and events are queryable, and their numbers behave like numbers.**
`metrics.k8s.io` reaches SQL as `pods_metrics_k8s_io` and
`nodes_metrics_k8s_io` alongside the events tables, so consumption, live spec
and failure reasons can be joined in one statement — a capacity and risk
review of a whole fleet as a single query.

Kubernetes reports measurements as strings with unit suffixes, which Postgres
cannot compare or sum: `WHERE cpu > '1'` is a string comparison that answers
wrongly without erroring. The new `axiom_quantity()` converts them exactly —
`100m` is `0.1`, `128Mi` is `134217728`, and `1M` is not `1Mi` — returning
`NULL` for anything malformed so one odd field cannot fail a cluster-wide
query.

The shipped gateway RBAC now grants `get`/`list`/`watch` cluster-wide, so API
groups installed after Axiom was deployed appear without an RBAC edit.
Mutating verbs stay enumerated per resource. A cluster-wide read includes
Secrets: `deploy/k8s/gateway-rbac.yaml` says what that means and how to narrow
it.
