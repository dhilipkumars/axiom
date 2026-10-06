# Axiom

**A Kubernetes foreign data wrapper for PostgreSQL. Query and control Kubernetes from plain SQL.**

[![CI](https://github.com/dhilipkumars/axiom/actions/workflows/ci.yml/badge.svg)](https://github.com/dhilipkumars/axiom/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/dhilipkumars/axiom?label=release)](https://github.com/dhilipkumars/axiom/releases/latest)
[![PostgreSQL](https://img.shields.io/badge/PostgreSQL-16%20%7C%2017%20%7C%2018-336791?logo=postgresql&logoColor=white)](https://dhilipkumars.github.io/axiom/compatibility/)
[![Docs](https://img.shields.io/badge/docs-dhilipkumars.github.io%2Faxiom-blue)](https://dhilipkumars.github.io/axiom/)
[![Security](https://img.shields.io/badge/scanned-govulncheck%20%C2%B7%20cargo--audit%20%C2%B7%20gitleaks-4c1)](https://github.com/dhilipkumars/axiom/actions/workflows/ci.yml)
[![License](https://img.shields.io/github/license/dhilipkumars/axiom)](LICENSE)

📖 **[Documentation](https://dhilipkumars.github.io/axiom/)** &nbsp;·&nbsp;
[Quick start](https://dhilipkumars.github.io/axiom/guides/quick-start/) &nbsp;·&nbsp;
[Install](https://dhilipkumars.github.io/axiom/guides/install/) &nbsp;·&nbsp;
[Examples](https://dhilipkumars.github.io/axiom/guides/examples/) &nbsp;·&nbsp;
[AI agent access](https://dhilipkumars.github.io/axiom/guides/agent-access/) &nbsp;·&nbsp;
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

### Which pods keep restarting?

`CrashLoopBackOff` is not a pod phase, and the restart count lives on each
container, a nested field `kubectl` cannot filter on.

```sql
SELECT namespace, name AS pod, phase, c->>'name' AS container,
       (c->>'restartCount')::int AS restarts,
       coalesce(c->'state'->'waiting'->>'reason',
                c->'lastState'->'terminated'->>'reason') AS why
  FROM k8s.core_pods,
       jsonb_array_elements(status->'containerStatuses') c
 WHERE (c->>'restartCount')::int > 0
 ORDER BY restarts DESC;
```

```
 namespace |       pod       |  phase  | container | restarts |        why        
-----------+-----------------+---------+-----------+----------+-------------------
 shop      | checkout-worker | Running | worker    |        3 | RunContainerError
(1 row)
```

### Did your patch regress?

Postgres 16, 17 and 18 clusters created by CloudNativePG from a table, a
`pgbench` Job against each, one at a time, and each run's throughput beside the
CPU and memory its Postgres used while it ran, all from SQL.

```sql
SELECT * FROM lab.results;
```

```
<!-- output:lab-results -->
```

**→ [The regression lab](https://dhilipkumars.github.io/axiom/guides/examples/regression-lab/)**
explains the `lab.results` view, how to read each column, and how to run it.

### Which customers are affected right now?

Axiom runs in your application's Postgres, so the cluster joins your own
tables, with no export in between.

```sql
CREATE TABLE tenants (namespace text PRIMARY KEY, customer text, plan text);
INSERT INTO tenants VALUES ('shop', 'Acme Corp', 'enterprise');

SELECT DISTINCT ON (p.namespace, p.name)
       t.customer, t.plan, p.name AS pod, e.reason
  FROM tenants t
  JOIN k8s.core_pods p ON p.namespace = t.namespace
  JOIN k8s.core_events e ON e.namespace = p.namespace
                        AND e.involved_object->>'kind' = 'Pod'
                        AND e.involved_object->>'name' = p.name
 WHERE e.type = 'Warning'
 ORDER BY p.namespace, p.name,
          coalesce(e.last_timestamp, e.event_time, e.creation_timestamp) DESC;
```

```
 customer  |    plan    |            pod            |      reason      
-----------+------------+---------------------------+------------------
 Acme Corp | enterprise | checkout-6b889d49cf-jt7bz | FailedScheduling
 Acme Corp | enterprise | checkout-worker           | BackOff
 Acme Corp | enterprise | report                    | Failed
(3 rows)
```

More, including writes, a capacity review and an operator written in SQL, in
**[Examples](https://dhilipkumars.github.io/axiom/guides/examples/)**.

## Getting started

One script brings up a local kind cluster, the gateway, and Postgres with Axiom,
then runs a first query. It needs Docker, `kind` and `kubectl`:

```sh
curl -fsSL https://github.com/dhilipkumars/axiom/releases/latest/download/quickstart.sh | bash
```

**→ [Quick start](https://dhilipkumars.github.io/axiom/guides/quick-start/)** has
the download-and-read version, and how to clean up.

Already run Postgres? [Install](https://dhilipkumars.github.io/axiom/guides/install/)
the gateway in your cluster and the extension from a `.deb` or `.rpm` per
Postgres major and architecture, no toolchain and no rebuild:

```sh
sudo apt install ./postgresql-17-axiom_<version>-1_amd64.deb     # Debian, Ubuntu
sudo dnf install ./axiom_17-<version>-1.el9.x86_64.rpm           # RHEL, Rocky, Alma 9
```

Axiom must be loaded through `shared_preload_libraries`; a package cannot do
that for you. [Compatibility](https://dhilipkumars.github.io/axiom/compatibility/)
lists the tested versions, distributions and architectures.

## How it works

A small Go **gateway** runs in each cluster and holds its credentials. The
**extension** in Postgres talks only to the gateway, over gRPC and TLS, and
keeps a watch-driven cache in shared memory. Postgres can live anywhere, and
what SQL can reach is exactly what the gateway's RBAC allows.

**→ [Architecture](https://dhilipkumars.github.io/axiom/architecture/)** has the design, and
**[How tables work](https://dhilipkumars.github.io/axiom/guides/examples/how-tables-work/)** what it means for a
query.

## Working on Axiom

- **[Developer guide](docs/development.md)** — toolchain, local stack, E2E
  gates, building each component, troubleshooting, repository layout.
- **[Contributing](CONTRIBUTING.md)** — what a change needs before it lands.
- **[Roadmap](ROADMAP.md)** — what is next, and what is deliberately not.
- **[Releasing](docs/RELEASING.md)** — how a version is cut and what each
  number means.

## Licence

Apache-2.0. See [LICENSE](LICENSE).
