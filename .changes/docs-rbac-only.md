---
kind: changed
---

**RBAC is now the source of truth for what a gateway exposes.** The guides
described two bounds — an allowlist and the ServiceAccount's RBAC — and told
you to keep them in step. Only one of them is enforced by the API server, so
the other was a way to be confused rather than a way to be safe. The guides
now teach RBAC alone: to change what appears in SQL, change the ClusterRole.

The `--serve` allowlist still exists and still works, as narrowing for a
gateway that should offer less than its ServiceAccount permits, but it is no
longer part of how Axiom is explained and is expected to be deprecated.

The deployment manifest therefore no longer sources `AXIOM_SERVE` from an
`axiom-gateway-config` ConfigMap; it is a literal that narrows nothing, and
nothing reads that ConfigMap any more.

Two operational claims were also wrong and are corrected. A kind removed from
the cluster does **not** stay on offer until the gateway restarts — resource
lists expire after `-discovery-ttl`, five minutes by default. Restarting is
required after an RBAC change, whose access decisions really are cached for
the process lifetime.

The README now links the documentation site from the top, and the guides
describe unshipped work as "on the roadmap" rather than by phase number, which
meant nothing to a reader who is not working on Axiom.
