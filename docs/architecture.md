# Architecture

Postgres never talks to the Kubernetes API server. A **gateway** runs inside
the cluster, and the **extension** in Postgres reaches it over gRPC and TLS.

```
 Kubernetes cluster                                Postgres host
 ┌──────────────────────────┐   gRPC over TLS   ┌─────────────────────────────────┐
 │ gateway (Go, in-cluster) │◄─────────────────►│ axiom (Postgres extension)      │
 │  client-go informers,    │                   │  background worker: one stream  │
 │  Get/List/Create/Update/ │                   │   per cluster                   │
 │  Delete/Subscribe        │                   │  shared-memory cache            │
 │  RBAC-bounded discovery  │                   │  foreign data wrapper per query │
 └──────────────────────────┘                   └─────────────────────────────────┘
```

## The two halves

**The gateway** is a stateless Go binary, deployed in the cluster as a
Deployment with a ServiceAccount. It is the only component holding cluster
credentials: the projected token kubelet mounts and rotates. It discovers what
its ServiceAccount may list, serves the OpenAPI schemas behind it, runs the
reads and writes, and streams watch events.

**The extension** is a Postgres foreign data wrapper written in Rust. A
background worker keeps one long-lived stream per cluster and fills a cache in
shared memory. Each query is served either from that cache or by a short call
to the gateway. No Kubernetes client code runs inside a Postgres backend.

## Why it is split this way

- **Postgres can live anywhere.** The database is often somewhere the cluster's
  private network does not reach: a managed host, another cloud, a laptop. The
  extension only ever dials out, so it needs no inbound connectivity, no
  kubeconfig and no credentials of its own.
- **One boundary for access.** What SQL can see is decided by the gateway's
  RBAC and nothing else. A `SELECT` cannot read anything the gateway's own
  identity could not read with `kubectl`, and Secrets are never granted by the
  shipped role.
- **Watch-driven, not polling.** A standing watch keeps the cache current, so a
  cached `SELECT` reflects the cluster within watch latency and costs the API
  server nothing. Tables opt in per kind.
- **Writes are real.** `INSERT`, `UPDATE` and `DELETE` become create, update
  and delete against the API server, carrying the `resourceVersion` they read,
  so a conflicting write fails instead of silently winning.
- **Multi-cluster by design.** One server per cluster and one schema per
  server, joined in one query. The cache is keyed by cluster.
- **No code per kind.** Discovery and the cluster's OpenAPI documents decide
  the tables and their column types, so a new CRD is a table on the next
  import.

## Deeper

The full reasoning, including the consistency tiers, the schema mapping and
the alternatives that were rejected, is in the
[design notes](DESIGN.md). [How tables work](guides/examples/how-tables-work.md)
covers what this means for a query: what is pushed down, how columns are
typed, and how caching behaves.
