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
outside the cluster it is querying.

## Why "Axiom"

Kubernetes is declarative. You do not instruct it to start a container; you
assert that one should be running, and controllers reconcile reality toward
that assertion. Its objects are axioms — facts the system takes as given and
works to make true — and what you read back is eventually consistent,
converging on what was asserted rather than reflecting it the instant you
write it.

Postgres is the opposite discipline: ACID, a consistent snapshot per
transaction, a commit that either happened or did not.

Axiom is the extension that bridges those two consistency models. It brings the
declarative, eventually-consistent view into a relational one, and is explicit
about where the seam falls: a `SELECT` reflects cluster state within watch
latency, and an `INSERT` is an assertion the cluster will reconcile, not a row
committed with your transaction. SQL over the cluster's own facts, without
either system pretending to be the other.

## Questions that need a query language

*More, including creating and scaling a CloudNativePG cluster from SQL, in
[Examples](https://dhilipkumars.github.io/axiom/guides/examples/). Which kinds
you can query is bounded by the gateway's RBAC — `nodes` and `deployments` below
need granting, which the examples page shows how to do.*

*Tables are named for their API group — `core_pods`, `apps_deployments`,
`postgresql_cnpg_io_clusters` — so a name never changes because something else
was installed in the cluster. If you would rather type `pods`,
`SELECT * FROM axiom_create_short_names('k8s')` creates short names as views.
[Table names](https://dhilipkumars.github.io/axiom/guides/querying/#table-names)
has the rules.*

**Join across kinds.** Which pods are running on nodes under memory pressure?

```sql
SELECT p.namespace, p.name, n.name AS node
FROM k8s.core_pods p
JOIN k8s.core_nodes n ON n.name = p.node
WHERE n.status->'conditions' @> '[{"type":"MemoryPressure","status":"True"}]';
```

**Aggregate.** Which namespaces are running the most non-Running pods, and why?

```sql
SELECT namespace, phase, count(*)
FROM k8s.core_pods
WHERE phase <> 'Running'
GROUP BY namespace, phase
ORDER BY count(*) DESC;
```

**Filter on anything, not just labels.** `kubectl` gives you label selectors and
a handful of field selectors. SQL gives you the whole object:

```sql
-- deployments that never finished rolling out
SELECT namespace, name, replicas, ready_replicas
FROM k8s.apps_deployments
WHERE coalesce(ready_replicas, '0')::int < replicas::int;
```

Promoted columns are `text` on purpose — an OpenAPI schema rarely constrains a
field tightly enough to justify a numeric column, so the cast is yours to make
and a missing field is `NULL` rather than `0`. Anything no column promotes is
still reachable through `raw`, the whole object as `jsonb`:

```sql
SELECT namespace, name
FROM k8s.apps_deployments
WHERE raw->'spec'->'template'->'spec'->'containers' @> '[{"imagePullPolicy":"Always"}]';
```

**Write.** This is the part most SQL-over-Kubernetes tools do not have — these
are real API calls, not a local cache being edited:

```sql
UPDATE k8s.core_configmaps
   SET data = data || '{"LOG_LEVEL":"debug"}'
 WHERE namespace = 'payments' AND name = 'api';

DELETE FROM k8s.core_configmaps WHERE namespace = 'staging' AND name = 'stale-flags';
```

Pods are deliberately read-only at the SQL layer, whatever RBAC allows — a
`DELETE` with a `WHERE` clause is too easy to get wrong and too hard to undo.

**Find crash-looping pods.** `CrashLoopBackOff` is a container waiting reason,
not a pod phase — so it lives in a nested field `kubectl` cannot filter on:

```sql
SELECT namespace, name
FROM k8s.core_pods
WHERE raw->'status'->'containerStatuses' @>
      '[{"state":{"waiting":{"reason":"CrashLoopBackOff"}}}]';
```

**Ask one question across many clusters**, because each cluster is its own
server and its own schema:

```sql
SELECT 'prod' AS cluster, namespace, name FROM prod.core_pods  WHERE phase = 'Failed'
UNION ALL
SELECT 'stage',           namespace, name FROM stage.core_pods WHERE phase = 'Failed';
```

## Getting started

Nothing is built from source. The walkthrough brings up a kind cluster, a TLS
keypair, the gateway, and a Postgres image with Axiom already in it, then
queries real cluster state — about ten minutes.

**→ [Getting started](https://dhilipkumars.github.io/axiom/guides/getting-started/)**

Already run Postgres? The same guide's
[package route](https://dhilipkumars.github.io/axiom/guides/getting-started/#installing-into-a-postgres-you-already-run)
installs the extension into it with `apt` or `dnf` — a `.deb` and an `.rpm` per
Postgres major and architecture, no toolchain and no rebuild:

```sh
sudo apt install ./postgresql-17-axiom_<version>-1_amd64.deb     # Debian, Ubuntu
sudo dnf install ./axiom_17-<version>-1.el9.x86_64.rpm           # RHEL, Rocky, Alma 9
```

Axiom must be loaded through `shared_preload_libraries`; a package cannot do
that for you. Artifacts are on the
[releases page](https://github.com/dhilipkumars/axiom/releases).

## Design

- **Watch-driven, not polling.** A standing watch keeps a shared-memory cache
  live, so a `SELECT` reflects cluster state within watch latency and costs the
  API server nothing. Tables opt in per kind with `cache_mode 'watch'`.
- **Postgres can live outside the cluster.** It always dials out, so it never
  needs inbound connectivity, a kubeconfig, or credentials of its own.
- **Multi-cluster by design.** One server per cluster, one schema per server,
  joins across them; the cache has been keyed by cluster since the first
  release. Isolation between two live clusters is not yet covered by a test —
  see the [roadmap](ROADMAP.md).
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
