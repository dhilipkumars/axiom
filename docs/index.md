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

It also means what Axiom can see is decided by one thing: the RBAC of the
ServiceAccount the gateway runs as. A `SELECT` cannot read anything the
gateway's own identity could not read with `kubectl`.

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

Axiom is under active development and has not reached a stable release.
Interfaces may still change between versions.

Everything described on this site works today. What it cannot do yet:

- **Act as the person running the query.** The gateway uses one identity for
  everyone, so what a `SELECT` can reach is decided by the gateway's RBAC, not
  the caller's. Grant the gateway only what every user of that database should
  be able to read.
- **Span more than one cluster.** A server points at a single gateway, which
  points at a single cluster. Querying several means several servers.

Both are on the roadmap. The engineering notes behind them live in the
repository, in `docs/DESIGN.md` and `docs/AUTH.md`.
