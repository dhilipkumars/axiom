# Roadmap

Tracked as issues, not duplicated here — this page goes stale the moment it
disagrees with them. **[All open issues](https://github.com/dhilipkumars/axiom/issues)**
is the authoritative list.

## Now

| | |
|---|---|
| [#65](https://github.com/dhilipkumars/axiom/issues/65) | **Extension upgrade path.** Axiom ships no `axiom--<from>--<to>.sql`, so `ALTER EXTENSION axiom UPDATE` has never worked and upgrading means `DROP EXTENSION ... CASCADE`, which takes every foreign table with it. The most user-visible gap we have. |
| [#66](https://github.com/dhilipkumars/axiom/issues/66) | **Documentation restructure.** A README that introduces the project rather than explaining how to build it, and guides split by install route. |

## The significant one: per-caller identity

**Today the gateway holds one ServiceAccount, and every Postgres role that can
`SELECT` from a foreign table gets all of it.** Two roles querying the same
table get identical results and identical write powers, whoever they are. There
is no caller identity on the wire at all — the extension sends a GVK and
filters, and the gateway acts as itself.

That is the gap that matters most before anyone runs this somewhere serious.
What exists today is the *gateway's* least privilege: discovery follows its own
RBAC, so it offers only the kinds it may list. What is missing is the
*caller's* — each SQL role reaching Kubernetes as a distinct, RBAC-scoped
identity, with the API server making the authorization decision rather than
Postgres being trusted to have made it.

[**docs/AUTH.md**](docs/AUTH.md) is the full design: the options considered and
rejected, the trust boundaries, the threat model, where an instance credential
can live, and an end-to-end flow. [PLAN.md's Phase 7](docs/PLAN.md) is the
delivery plan.

**It comes before multi-cluster on purpose.** `CREATE USER MAPPING` is the
per-cluster credential mechanism, so building multi-cluster first would leave
every registered cluster sharing one ambient trust relationship — and multiply
the blast radius of a weak trust model from one cluster to N. mTLS is also far
cheaper to get right against one gateway than several.

## Then

| | |
|---|---|
| **Multi-cluster** ([PLAN.md Phase 8](docs/PLAN.md)) | One Postgres, several clusters, each a server and a schema. The mechanism is in place — cache keyed by cluster, schema-per-cluster naming settled — so what remains is proving *isolation*: a failure in one cluster's stream must not affect another's. That plus the credential story above. |
| [#30](https://github.com/dhilipkumars/axiom/issues/30) | **How subscription slots scale** beyond the fixed 64. Needs measurement before design — which is itself blocked on Axiom having no metrics about itself. |
| [#34](https://github.com/dhilipkumars/axiom/issues/34) | **`--serve` becomes dev-only**, rather than a deployment option that quietly changes what a gateway offers. |

## Being considered

| | |
|---|---|
| [#26](https://github.com/dhilipkumars/axiom/issues/26) | **Built-in change history.** The watch worker already sees every ADDED/MODIFIED/DELETED with its `resourceVersion`, at no cost to the API server. Nothing else has that feed, which is what makes storing it defensible where storing metrics is not. |
| — | **`metrics.k8s.io` as foreign tables.** `kubectl top` joined to cluster state. It is another Kubernetes API group, so it would fall out of the existing discovery machinery rather than needing new architecture. |
| — | **Self-instrumentation.** Cache hit rate, watch lag, RPC latency, slot occupancy. Small, and it unblocks #30. |

## Not planned

- **Ingesting metrics continuously.** Axiom runs *inside* your database, so an
  unbounded write stream fails as table bloat and WAL growth in production
  Postgres. Prometheus already does this better; querying it is interesting,
  storing it is not.
- **Receiving OTLP.** That inverts a pull-based reader into a push target, with
  the authentication, backpressure and durability that implies.
- **A hosted apt/yum repository.** Needs a signing key and permanent key
  management. Release assets get you there without either.
