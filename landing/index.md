---
title: "Axiom: A Kubernetes Foreign Data Wrapper (kubernetes_fdw) for PostgreSQL"
description: "Axiom is a Kubernetes foreign data wrapper for PostgreSQL. Query and change Kubernetes resources, built-in kinds and CRDs, directly from SQL."
hide:
  - navigation
  - toc
---

# Axiom: a Kubernetes foreign data wrapper for PostgreSQL

**Query and control Kubernetes from plain SQL.**

Axiom is a Kubernetes foreign data wrapper for PostgreSQL: an extension that
makes Kubernetes resources, built-in kinds and custom resources alike, look
like tables. If you were looking for a `kubernetes_fdw`, this is it.

`SELECT` reads the live cluster. `INSERT`, `UPDATE` and `DELETE` are real
Kubernetes writes, with conflicts surfaced as SQL errors. The Postgres doing
the querying can live entirely outside the cluster it queries.

```sql
SELECT d.name, d.replicas, d.ready_replicas
  FROM k8s.apps_deployments d
  JOIN service_owners o ON o.service = d.name
 WHERE d.namespace = 'production'
   AND d.ready_replicas < d.replicas;
```

## Read the docs

- **[Latest release](latest/)**: the docs for what you can install today.
- **[dev](dev/)**: the docs for unreleased work on `main`.

Every docs page has a version picker at the top, to move between releases.

## Try it

One script brings up a local kind cluster, the gateway, and Postgres with
Axiom, then runs a first query. It needs Docker, `kind` and `kubectl`:

```sh
curl -fsSL https://github.com/dhilipkumars/axiom/releases/latest/download/quickstart.sh | bash
```

## How it works

A small **gateway** runs in each cluster and holds its credentials. The
**extension** in Postgres talks only to the gateway, over gRPC and TLS, and
keeps a watch-driven cache in shared memory. What SQL can reach is exactly
what the gateway's RBAC allows.

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

The source, releases and roadmap are on
[GitHub](https://github.com/dhilipkumars/axiom).
