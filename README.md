# Axiom

**Query and control Kubernetes from plain SQL.**

[![CI](https://github.com/dhilipkumars/axiom/actions/workflows/ci.yml/badge.svg)](https://github.com/dhilipkumars/axiom/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/dhilipkumars/axiom?label=release)](https://github.com/dhilipkumars/axiom/releases/latest)
[![PostgreSQL](https://img.shields.io/badge/PostgreSQL-16%20%7C%2017%20%7C%2018-336791?logo=postgresql&logoColor=white)](https://dhilipkumars.github.io/axiom/guides/getting-started/)
[![Docs](https://img.shields.io/badge/docs-dhilipkumars.github.io%2Faxiom-blue)](https://dhilipkumars.github.io/axiom/)
[![Security](https://img.shields.io/badge/scanned-govulncheck%20%C2%B7%20cargo--audit%20%C2%B7%20gitleaks-4c1)](https://github.com/dhilipkumars/axiom/actions/workflows/ci.yml)
[![License](https://img.shields.io/github/license/dhilipkumars/axiom)](LICENSE)

📖 **[Documentation](https://dhilipkumars.github.io/axiom/)** &nbsp;·&nbsp;
[Getting started](https://dhilipkumars.github.io/axiom/guides/getting-started/) &nbsp;·&nbsp;
[Querying](https://dhilipkumars.github.io/axiom/guides/querying/) &nbsp;·&nbsp;
[Deploying](https://dhilipkumars.github.io/axiom/guides/deploying/) &nbsp;·&nbsp;
[Roadmap](ROADMAP.md)

Axiom makes Kubernetes resources — built-in kinds and CRDs alike — look like
tables in Postgres. `SELECT` reads the live cluster. `INSERT`, `UPDATE` and
`DELETE` are real Kubernetes writes, with optimistic-concurrency conflicts
surfaced as SQL errors. The Postgres doing the querying can live entirely
outside the cluster it is querying, and can query several clusters at once.

## Why "Axiom"

An axiom is something a system takes as given and reasons from. Kubernetes
already holds the facts about your infrastructure — what is running, where,
owned by what, in what state. Axiom stops treating those facts as something you
fetch and start parsing, and starts treating them as something you **query**:
a relational surface over the cluster's own truth, in the database you already
use for everything else.

The name is also a small joke about the two halves. **Ax**iom sits between the
Kubernetes **API** and Postgres's relational mod**el** — a foreign data wrapper
is exactly the mechanism Postgres provides for saying "this data lives
somewhere else, here is how to reason about it as if it did not."

## Things you cannot do with kubectl

**Join across kinds.** Which pods are running on nodes under memory pressure?

```sql
SELECT p.namespace, p.name, n.name AS node
FROM k8s.pods p
JOIN k8s.nodes n ON n.name = p.node
WHERE n.status->'conditions' @> '[{"type":"MemoryPressure","status":"True"}]';
```

**Aggregate.** Which namespaces are running the most non-Running pods, and why?

```sql
SELECT namespace, phase, count(*)
FROM k8s.pods
WHERE phase <> 'Running'
GROUP BY namespace, phase
ORDER BY count(*) DESC;
```

**Filter on anything, not just labels.** `kubectl` gives you label selectors and
a handful of field selectors. SQL gives you the whole object:

```sql
-- deployments that never finished rolling out
SELECT namespace, name, replicas, ready_replicas
FROM k8s.deployments
WHERE coalesce(ready_replicas, '0')::int < replicas::int;
```

Promoted columns are `text` on purpose — an OpenAPI schema rarely constrains a
field tightly enough to justify a numeric column, so the cast is yours to make
and a missing field is `NULL` rather than `0`. Anything no column promotes is
still reachable through `raw`, the whole object as `jsonb`:

```sql
SELECT namespace, name
FROM k8s.deployments
WHERE raw->'spec'->'template'->'spec'->'containers' @> '[{"imagePullPolicy":"Always"}]';
```

**Write.** This is the part most SQL-over-Kubernetes tools do not have — these
are real API calls, not a local cache being edited:

```sql
UPDATE k8s.configmaps
   SET data = data || '{"LOG_LEVEL":"debug"}'
 WHERE namespace = 'payments' AND name = 'api';

DELETE FROM k8s.pods WHERE namespace = 'staging' AND phase = 'Failed';
```

**Ask one question across many clusters**, because each cluster is just another
schema:

```sql
SELECT 'prod' AS cluster, name, phase FROM prod.pods   WHERE phase = 'CrashLoopBackOff'
UNION ALL
SELECT 'stage',           name, phase FROM stage.pods  WHERE phase = 'CrashLoopBackOff';
```

## Getting started

Run a Postgres image with Axiom already in it, deploy the gateway, and query:

```sh
kubectl apply -f https://raw.githubusercontent.com/dhilipkumars/axiom/main/deploy/k8s/gateway-rbac.yaml
kubectl apply -f https://raw.githubusercontent.com/dhilipkumars/axiom/main/deploy/k8s/gateway-deployment.yaml

docker run -d --name axiom-postgres \
  -e POSTGRES_PASSWORD=axiom -p 55432:5432 \
  ghcr.io/dhilipkumars/axiom-postgres:latest-pg17
```

Already run Postgres? Install the extension into it instead — a `.deb` or
`.rpm` per major and architecture, no toolchain and no rebuild:

```sh
sudo apt install ./postgresql-17-axiom_<version>-1_amd64.deb     # Debian, Ubuntu
sudo dnf install ./axiom_17-<version>-1.el9.x86_64.rpm           # RHEL, Rocky, Alma 9
```

**→ [Full walkthrough](https://dhilipkumars.github.io/axiom/guides/getting-started/)**,
including the TLS keypair, the `shared_preload_libraries` step Axiom requires,
and the download links. Artifacts are on the
[releases page](https://github.com/dhilipkumars/axiom/releases).

## What makes it different

- **Watch-driven, not polling.** A standing watch keeps a shared-memory cache
  live, so a `SELECT` reflects cluster state within watch latency and costs the
  API server nothing. Tables opt in per kind with `cache_mode 'watch'`.
- **Postgres can live outside the cluster.** It always dials out, so it never
  needs inbound connectivity, a kubeconfig, or credentials of its own.
- **Multi-cluster from the start.** One server per cluster, one schema per
  server, joins across them.
- **Bounded by RBAC, not by configuration.** The gateway offers exactly the
  kinds its ServiceAccount may list. Discovery finds CRDs with no code change.
- **Writes are real.** `INSERT`/`UPDATE`/`DELETE` become create/update/delete
  against the API server, and a conflicting write raises a SQL error rather
  than silently winning.

## How it works

No Kubernetes client code ever runs inside a Postgres backend. A small **Go
gateway** runs in each cluster, holds that cluster's credentials, and exposes a
gRPC API over TLS. The **pgrx extension** talks only to gateways: a background
worker keeps one persistent stream per cluster and maintains a shared-memory
cache, while per-connection backends serve scans from that cache or issue short
unary RPCs.

```
 Kubernetes cluster                                Postgres host
 ┌──────────────────────────┐   gRPC over TLS   ┌─────────────────────────────────┐
 │ gateway (Go, in-cluster) │◄─────────────────►│ axiom (pgrx extension)          │
 │  client-go informers,    │                   │  bgworker: tokio + tonic client │
 │  Get/List/Create/Update/ │                   │  shared-memory cache (dshash)   │
 │  Delete/Subscribe        │                   │  FDW callbacks per backend      │
 └──────────────────────────┘                   └─────────────────────────────────┘
```

The reasoning behind that split — why a gateway rather than a client in the
backend, the consistency tiers, the schema mapping — is in
[docs/DESIGN.md](docs/DESIGN.md).

## Working on Axiom

- **[Developer guide](docs/development.md)** — toolchain, local stack, E2E
  gates, building each component, troubleshooting, repository layout.
- **[Contributing](CONTRIBUTING.md)** — what a change needs before it lands.
- **[Roadmap](ROADMAP.md)** — what is next, and what is deliberately not.
- **[Releasing](docs/RELEASING.md)** — how a version is cut and what each
  number means.

## Licence

Apache-2.0. See [LICENSE](LICENSE).
