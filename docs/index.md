# Axiom

Axiom lets you query and change a Kubernetes cluster with SQL.

It is a Postgres foreign data wrapper. Kinds become foreign tables, so a Pod is
a row, a label selector is a `WHERE` clause, and joining what a cluster knows
against what your own database knows is one query instead of a script.

```sql
SELECT d.name, d.replicas, d.ready_replicas
  FROM k8s.deployments d
  JOIN service_owners o ON o.service = d.name
 WHERE d.namespace = 'production'
   AND d.ready_replicas::int < d.replicas::int;
```

## The shape of it

Postgres never talks to the Kubernetes API server. A **gateway** runs inside
the cluster and speaks gRPC over TLS to the extension loaded into Postgres.

That split is the whole design, and it exists because the database is very
often somewhere the cluster's private network does not reach: a managed
Postgres, a different cloud, a laptop. The gateway is the only component that
needs cluster credentials, and it holds them as a ServiceAccount rather than as
anything Postgres stores.

It also means what Axiom can see is bounded twice: by the gateway's `-serve`
list, and by the RBAC of the ServiceAccount it runs as. A `SELECT` cannot read
anything the gateway's own identity could not read with `kubectl`.

## What it does today

- **Read** any kind the gateway serves, including custom resources, with
  namespace and name filters pushed down to the API server rather than applied
  after fetching everything.
- **Write** with `INSERT`, `UPDATE` and `DELETE`, using the API server's own
  optimistic concurrency. A conflicting `UPDATE` is a retryable `40001`.
- **Discover** schemas. `IMPORT FOREIGN SCHEMA` reads the cluster's OpenAPI
  documents and generates a table per kind, so a CRD needs no code.
- **Cache** with watches. A table in `cache_mode 'watch'` is served from a
  shared-memory cache kept current by a watch stream, and says so loudly with a
  `WARNING` when the stream is degraded and the rows are stale.

## Where to start

- [Getting started](guides/getting-started.md) — a cluster, a gateway and a
  first query.
- [Deploying the gateway](guides/deploying.md) — running it as a Deployment,
  and the faster loop for developing it.
- [Querying](guides/querying.md) — what pushes down, what does not, and how
  caching behaves.

The reference pages under **Reference** are generated from the code itself.

## Status

Axiom is under active development and has not reached a stable release. Phases
0 through 6 are complete; per-caller identity (Phase 7) and multi-cluster
(Phase 8) are next. See `docs/PLAN.md` in the repository for the current plan
and `docs/DESIGN.md` for the architecture.
