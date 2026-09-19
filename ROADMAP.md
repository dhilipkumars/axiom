# Roadmap

Tracked as issues, not duplicated here — this page goes stale the moment it
disagrees with them. **[All open issues](https://github.com/dhilipkumars/axiom/issues)**
is the authoritative list.

## Now

| | |
|---|---|
| [#65](https://github.com/dhilipkumars/axiom/issues/65) | **Extension upgrade path.** Axiom ships no `axiom--<from>--<to>.sql`, so `ALTER EXTENSION axiom UPDATE` has never worked and upgrading means `DROP EXTENSION ... CASCADE`, which takes every foreign table with it. The most user-visible gap we have. |
| [#66](https://github.com/dhilipkumars/axiom/issues/66) | **Documentation restructure.** A README that introduces the project rather than explaining how to build it, and guides split by install route. |

## Next

| | |
|---|---|
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
