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
reads that ConfigMap any more.

**Upgrading needs care if you narrowed the serve list.** Applying the new
manifest replaces the `valueFrom` that read your ConfigMap with the literal
`*.*`, so the gateway begins offering every kind its RBAC permits. That is a
widening, and nothing warns about it. If you relied on the serve list to offer
less than RBAC allows, re-apply the narrowing after the manifest:

```sh
kubectl -n axiom-system set env deploy/axiom-gateway AXIOM_SERVE='pods,configmaps'
```

Better, move the bound into RBAC, which the API server enforces. Deployments
that never narrowed the list are unaffected.

The README now links the documentation site from the top, and the guides
describe unshipped work as "on the roadmap" rather than by phase number, which
meant nothing to a reader who is not working on Axiom.
