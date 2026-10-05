---
title: "Axiom: A Kubernetes Foreign Data Wrapper (kubernetes_fdw) for PostgreSQL"
description: "Axiom is a Kubernetes foreign data wrapper for PostgreSQL. Query and change Kubernetes resources, built-in kinds and CRDs, directly from SQL."
---

# Axiom: a Kubernetes foreign data wrapper for PostgreSQL

**Query and control Kubernetes from plain SQL.**

Axiom is a Kubernetes foreign data wrapper for PostgreSQL: an extension that
makes Kubernetes resources, built-in kinds and custom resources alike, look
like tables. If you were looking for a `kubernetes_fdw`, this is it.

`SELECT` reads the live cluster. `INSERT`, `UPDATE` and `DELETE` are real
Kubernetes writes, with conflicts surfaced as SQL errors. The Postgres doing the querying can live entirely
outside the cluster it queries.

```sql
SELECT d.name, d.replicas, d.ready_replicas
  FROM k8s.apps_deployments d
  JOIN service_owners o ON o.service = d.name
 WHERE d.namespace = 'production'
   AND d.ready_replicas < d.replicas;
```

## Why

`kubectl` answers one question about one kind at a time. The questions people
actually have cross kinds and leave the cluster: which customers are affected
by the pods failing right now, which rollouts are stuck and why, which
workloads reserve memory they never use. Today those are scripts that pipe
`kubectl` into `jq` into a spreadsheet.

With Axiom they are queries. A Pod is a row, a label selector is a `WHERE`
clause, and joining what the cluster knows against what your own database
knows is one statement, because the cluster's tables sit beside yours.

## Query Kubernetes from PostgreSQL

- **Read** any kind the gateway may list, custom resources included, with
  namespace and name filters pushed down to the API server.
- **Write** with `INSERT`, `UPDATE` and `DELETE`, using the API server's own
  optimistic concurrency. A conflicting `UPDATE` is a retryable `40001`.
- **Discover** schemas. `IMPORT FOREIGN SCHEMA` reads the cluster's OpenAPI
  documents and creates a typed table per kind, so a CRD needs no code.
- **Cache** with watches. A table can be served from a shared-memory cache that
  a watch stream keeps current, and it says so with a `WARNING` when the rows
  are stale.
- **Measure.** Usage from metrics-server and events are tables too, and
  `axiom_quantity()` turns `500m` and `128Mi` into numbers you can sum.

## How the Kubernetes foreign data wrapper works

A small **gateway** runs in the cluster and holds its credentials. The
**extension** in Postgres talks only to the gateway, over gRPC and TLS, and
never to the API server. What a query can reach is exactly what the gateway's
ServiceAccount may read. [Architecture](architecture.md) has the design and why
it is split that way.

## Why "Axiom"

Kubernetes is declarative. You do not instruct it to start a container; you
assert that one should be running, and controllers reconcile reality toward
that assertion. Its objects are axioms: facts the system takes as given and
works to make true.

Postgres is the opposite discipline: a consistent snapshot per transaction, a
commit that either happened or did not. Axiom bridges the two and is explicit
about the seam. A `SELECT` reflects cluster state within watch latency, and an
`INSERT` is an assertion the cluster will reconcile, not a row committed with
your transaction.

## Installation

- **[Quick start](guides/quick-start.md)**: one script brings up a kind
  cluster, the gateway and Postgres with Axiom, and runs a first query.
- **[Install](guides/install/index.md)**: the gateway, then Axiom into Postgres
  by image, package or source.
- **[Examples](guides/examples/index.md)**: questions `kubectl` cannot answer
  in one command, each with its real output.

## Status

Axiom is young and moving quickly, and interfaces may change between minor
versions. Everything on this site works today. What it cannot do yet:

- **Act as the person running the query.** The gateway uses one identity for
  everyone, so grant it only what every user of that database may read.
- **Span clusters in one server.** A server points at one gateway and one
  cluster; several clusters means several servers, which one query can join.

Both are on the [roadmap](https://github.com/dhilipkumars/axiom/blob/main/ROADMAP.md).
