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

The shipped gateway RBAC now reads broadly: everything in Kubernetes' `view`
role (workloads, ConfigMaps, Services, NetworkPolicies, Ingresses, Events),
plus nodes, storage, CRDs, RBAC objects, `events.k8s.io` and the resource,
custom and external metrics APIs. **Secrets are excluded, and cannot be
reached through it.** Custom resources are included when their operator ships
an `aggregate-to-view` role, or when you label a read-only ClusterRole with
`axiom.dhilipkumars.github.io/aggregate-to-gateway: "true"`. Mutating verbs
stay enumerated per resource.
