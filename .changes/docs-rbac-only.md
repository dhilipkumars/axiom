---
kind: changed
---

The published guides now describe one bound on what a gateway serves, not two.
`--serve` defaults to `*.*`, so RBAC alone decides, and teaching a second
allowlist alongside it made the first deployment look harder than it is and
gave two places to get wrong. The flag is unchanged and still documented under
the gateway flag reference, as narrowing for a gateway that should offer less
than its ServiceAccount permits.

Consequently the deployment manifest no longer sources `AXIOM_SERVE` from an
`axiom-gateway-config` ConfigMap; it is a literal in the manifest, and nothing
reads that ConfigMap any more. Note that applying the new manifest replaces
the `valueFrom` that read it with the literal `*.*`, so a gateway narrowed
through that ConfigMap starts offering everything its RBAC permits; narrow it
with `kubectl set env` on the Deployment, or better, in RBAC.

The README now links the documentation site from the top, and the guides
describe unshipped work as "on the roadmap" rather than by phase number, which
meant nothing to a reader who is not working on Axiom.
