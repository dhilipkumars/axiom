# Axiom

**A Postgres foreign data wrapper for Kubernetes.**

Axiom lets you query and control Kubernetes resources, built-in kinds and CRDs
alike, from plain SQL, across one or more clusters, from a Postgres instance
that may live entirely outside those clusters' networks. It goes beyond
point-in-time polling: a standing watch keeps a shared-memory cache live so
`SELECT`s reflect cluster state within watch latency, and `INSERT`/`UPDATE`/
`DELETE` map to real Kubernetes create/update/delete with optimistic-concurrency
conflicts surfaced as SQL errors.

```sql
CREATE SERVER prod FOREIGN DATA WRAPPER axiom_fdw OPTIONS (endpoint 'https://gw.prod.example:8443');
IMPORT FOREIGN SCHEMA k8s FROM SERVER prod INTO prod;

SELECT name, namespace, phase FROM prod.pods WHERE namespace = 'payments' AND phase <> 'Running';
UPDATE prod.configmaps SET data = data || '{"LOG_LEVEL":"debug"}' WHERE namespace = 'payments' AND name = 'api';
```

That is the end goal. Read the three design documents for the full picture:

| Document | What it covers |
|---|---|
| [docs/DESIGN.md](docs/DESIGN.md) | Architecture, why a gateway, consistency tiers, schema mapping, multi-cluster, auth |
| [docs/PLAN.md](docs/PLAN.md) | Six phases from plumbing to hardened multi-cluster, each with its own E2E test |
| [docs/RULES.md](docs/RULES.md) | Engineering gates every phase must meet: quality, testability, security, E2E |

## How it works (one paragraph)

No Kubernetes client code ever runs inside a Postgres backend. A small **Go
gateway** runs inside each cluster, holds the cluster credentials, and exposes a
gRPC API over TLS. The **pgrx extension** in Postgres talks only to gateways: a
background worker keeps one persistent stream per cluster and maintains a
shared-memory cache; per-connection backends serve scans from that cache or
issue short unary RPCs. Postgres always dials out, so it never needs inbound
connectivity from the cluster.

```
 Kubernetes cluster                                Postgres host
 ┌──────────────────────────┐   gRPC over TLS   ┌────────────────────────────────┐
 │ gateway (Go, in-cluster) │◄─────────────────►│ axiom (pgrx extension)          │
 │  client-go informers,    │                   │  bgworker: tokio + tonic client │
 │  Get/List/Create/Update/ │                   │  shared-memory cache (dshash)   │
 │  Delete/Subscribe        │                   │  FDW callbacks per backend      │
 └──────────────────────────┘                   └────────────────────────────────┘
```

## Current status: Phase 1 (read-only Pods, on-demand)

Phase 0 proved the Postgres ↔ gateway boundary; Phase 1 puts the first real
Kubernetes data behind SQL. There is no cache or watch yet (Phase 3) and no
writes (Phase 2). What exists and is tested end to end:

| Component | Path | What it does today |
|---|---|---|
| Protobuf API | [proto/axiom/v1/axiom.proto](proto/axiom/v1/axiom.proto) | `Ping`, `Get`, `List` (generic GVK + raw object JSON) |
| Gateway (Go) | [gateway/](gateway/) | TLS-only gRPC server; `Get`/`List` over `client-go`'s dynamic client behind a narrow interface; Pods only; runs under a least-privilege ServiceAccount ([deploy/k8s/gateway-rbac.yaml](deploy/k8s/gateway-rbac.yaml)) |
| Extension (Rust, pgrx) | [extension/](extension/) | `axiom_fdw` foreign data wrapper: `CREATE SERVER` + `CREATE FOREIGN TABLE ... OPTIONS (resource 'pods')`, `namespace`/`name` qual pushdown, typed columns plus `raw jsonb`, FDW SQLSTATEs on failure; plus the Phase 0 background pinger |
| Local stack | [deploy/compose/](deploy/compose/) | Base stack (no cluster) and a kind overlay that gives the gateway a kubeconfig |
| E2E | [e2e/ping_test.sh](e2e/ping_test.sh), [e2e/pods_test.sh](e2e/pods_test.sh) | Phase 0 and Phase 1 gates on the shared setup libraries in [e2e/lib/](e2e/lib/) |

```sql
CREATE EXTENSION axiom;
CREATE SERVER kind FOREIGN DATA WRAPPER axiom_fdw
  OPTIONS (endpoint 'https://gateway:8443', ca_cert '/certs/ca.crt');
CREATE FOREIGN TABLE k8s_pods (name text, namespace text, phase text, node text, raw jsonb)
  SERVER kind OPTIONS (resource 'pods');

SELECT name, phase, node FROM k8s_pods WHERE namespace = 'kube-system';
SELECT raw->'metadata'->>'uid' FROM k8s_pods WHERE namespace = 'default' AND name = 'web-0';
```

The rest of this README walks you through building, running, and testing it.

## 1. Prerequisites

You need Docker for the quickest path (section 3). For building and unit-testing
the components natively you also need the Go and Rust toolchains.

### Quick path (E2E only)

- Docker Desktop or Docker Engine with Compose v2 (`docker compose version`)
- `bash`, `git`, `make`
- For the Phase 1 test: [`kind`](https://kind.sigs.k8s.io) and `kubectl`

### Full developer setup

**Go side**

```sh
# Go 1.26+  (https://go.dev/dl); client-go v0.37 requires it
go version

# buf (proto lint + codegen) and golangci-lint v2
go install github.com/bufbuild/buf/cmd/buf@latest
go install github.com/golangci/golangci-lint/v2/cmd/golangci-lint@latest
```

**Rust side**

```sh
# Rust stable via rustup (https://rustup.rs)
rustc --version

# PostgreSQL 14–17 with server headers. Examples:
#   macOS:   brew install postgresql@16
#   Debian:  apt install postgresql-16 postgresql-server-dev-16
# pgrx also needs clang/libclang for bindgen (brew install llvm / apt install clang libclang-dev)

cargo install cargo-pgrx --version 0.12.9 --locked
cargo install cargo-audit cargo-deny --locked

# Tell pgrx which Postgres to use (pick the major you installed):
cargo pgrx init --pg16 "$(which pg_config)"
```

Check what pgrx knows about with `cat ~/.pgrx/config.toml`. The `make` targets
below take `PG=pg16` (or `pg14`, `pg15`, `pg17`) to match.

## 2. Get the code

```sh
git clone https://github.com/dhilipkumars/axiom.git
cd axiom
git checkout phase-1      # until the Phase 1 PR is merged into main
```

## 3. Run the E2E gates (the thing to try first)

Both are fully automated. Phase 0 needs only Docker:

```sh
make e2e-ping        # Phase 0 gate (alias: make e2e-phase0), Docker only
make e2e-pods        # Phase 1 gate (alias: make e2e-phase1), needs kind + kubectl
make e2e-configmaps  # Phase 2 gate (alias: make e2e-phase2), needs kind + kubectl
make e2e             # all gates, oldest first
```

Phase 1 also needs `kind` and `kubectl`; it creates a cluster named `axiom-e2e`,
applies the least-privilege RBAC and three fixture Pods, and deletes the
cluster afterwards:

```sh
make e2e-pods        # alias: make e2e-phase1
make e2e             # both gates, oldest first
```

The first run builds two images and takes several minutes (it compiles
`cargo-pgrx` and the extension inside Docker). Subsequent runs reuse build
caches and take about a minute. You should see, for the pods test:

```
==> creating kind cluster axiom-e2e
==> applying least-privilege gateway RBAC
==> building and starting stack
==> applying fixture pods and waiting for Ready
==> defining server and foreign table
==> namespace scan matches kubectl
db-0|Running
web-0|Running
web-1|Running
==> point get matches kubectl (name, node, uid via raw jsonb)
==> quals were pushed down to the gateway (namespace + name), not filtered locally
==> local (non-pushed) quals still apply: phase filter and label via raw
==> nonexistent pod is an empty result, not an error
==> RBAC is least-privilege: the gateway identity cannot read secrets or list namespaces
==> gateway down: SELECT raises fdw_unable_to_establish_connection, then recovers
==> PODS E2E PASSED
```

Knobs: `E2E_TIMEOUT_SECS=120` to wait longer on a slow machine, `E2E_KEEP=1` to
leave the compose stack running afterwards so you can poke at it (see next section),
`E2E_KIND_KEEP=1` to keep the kind cluster, `E2E_NO_BUILD=1` to reuse already-built images.

## 4. Poke at the running stack by hand

Bring the stack up and leave it running. With the pods test you get a real
cluster behind it:

```sh
E2E_KEEP=1 E2E_KIND_KEEP=1 make e2e-pods
# or, without a cluster (Ping only):
make up
```

Open a SQL session inside the Postgres container:

```sh
docker compose -f deploy/compose/docker-compose.yml exec postgres psql -U axiom -d axiom
```

Things worth trying:

```sql
-- Install the extension's SQL objects (idempotent)
CREATE EXTENSION IF NOT EXISTS axiom;
SELECT axiom_version();

-- The settings the background worker is using
SHOW axiom.gateway_endpoint;
SHOW axiom.gateway_ca_cert;
SHOW axiom.ping_interval_secs;
SHOW axiom.rpc_timeout_secs;

-- The worker is a real Postgres process, visible like any backend
SELECT pid, backend_type, backend_start FROM pg_stat_activity WHERE backend_type = 'axiom gateway pinger';

-- With the kind stack: query the cluster (the E2E already created server + table)
SELECT name, phase, node FROM k8s_pods WHERE namespace = 'kube-system' ORDER BY 1;
SELECT name, raw->'status'->>'podIP' FROM k8s_pods WHERE namespace = 'axiom-e2e' AND name = 'web-0';
EXPLAIN SELECT name FROM k8s_pods WHERE namespace = 'axiom-e2e';   -- plans without contacting the gateway
```

Watch the gateway log to see pushdown in action: each `List` logs the namespace
and name filters it received:

```sh
docker compose -f deploy/compose/docker-compose.yml logs -f gateway | grep '"msg":"list"'
```

Query a cluster. Point the stack at a kind cluster with the Phase 1/2 harness
(`E2E_KEEP=1 E2E_KIND_KEEP=1 ./e2e/pods_test.sh` leaves everything running), then:

```sql
CREATE SERVER kind FOREIGN DATA WRAPPER axiom_fdw
  OPTIONS (endpoint 'https://gateway:8443', ca_cert '/certs/ca.crt', rpc_timeout_secs '10');

CREATE FOREIGN TABLE k8s_pods (name text, namespace text, phase text, node text, raw jsonb)
  SERVER kind OPTIONS (resource 'pods');
CREATE FOREIGN TABLE k8s_configmaps (name text, namespace text, data jsonb, raw jsonb)
  SERVER kind OPTIONS (resource 'configmaps');

SELECT name, phase, node FROM k8s_pods WHERE namespace = 'kube-system';          -- namespace/name are pushed down
INSERT INTO k8s_configmaps (name, namespace, data) VALUES ('app', 'default', '{"LOG_LEVEL":"info"}');
UPDATE k8s_configmaps SET data = data || '{"LOG_LEVEL":"debug"}' WHERE namespace = 'default' AND name = 'app';
DELETE FROM k8s_configmaps WHERE namespace = 'default' AND name = 'app';
```

Any subset of a kind's columns may be declared, but UPDATE/DELETE need the
`raw jsonb` column: it carries the object's identity and the `resourceVersion`
the row was read at. If the object changed between your read and your write,
the statement fails with SQLSTATE `40001` (`serialization_failure`); re-read and
retry, exactly as you would for a serialization failure on a local table.
Kubernetes has no transactions, so a write takes effect when the statement
runs and is not undone by `ROLLBACK`. Pods are read-only.

Watch the worker's log lines from another terminal:

```sh
docker compose -f deploy/compose/docker-compose.yml logs -f postgres | grep "axiom bgworker"
```

Now break things and watch it cope. Stop the gateway and you should see
`ping failed ... code=Unavailable` at `WARNING` with the retry delay doubling
from 1s up to 60s; start it again and the next attempt logs `ping ok` and the
interval resets:

```sh
docker compose -f deploy/compose/docker-compose.yml stop gateway
# ...watch the warnings and backoff...
docker compose -f deploy/compose/docker-compose.yml start gateway
```

Settings are reloadable without a restart. For example, shrink the ping interval:

```sql
ALTER SYSTEM SET axiom.ping_interval_secs = 5;
SELECT pg_reload_conf();
```

Tear everything down (this also deletes the generated certificates and, if you
kept it, the kind cluster):

```sh
make down
kind delete cluster --name axiom-e2e
```

## 5. Build and test the components natively

Each RULES.md gate has its own `make` target so a failure localises to one side
of the gRPC boundary.

```sh
# Protobuf: lint, regenerate Go stubs, and fail if generated code drifted
make proto
make proto-check

# Gateway (Go)
make gateway-build
make gateway-test        # go test -race, table-driven, incl. error paths and an in-process gRPC round-trip
make gateway-lint        # golangci-lint: errcheck, govet, staticcheck, gosec, ...
make gateway-vuln        # govulncheck

# Extension (Rust / pgrx). PG must match a `cargo pgrx init`-ed version.
make ext-build PG=pg16
make ext-lint  PG=pg16   # cargo fmt --check + clippy::pedantic with -D warnings
make ext-test  PG=pg16   # pure unit tests + tests against a real, temporary Postgres
make ext-audit           # cargo audit + cargo deny (advisories, licenses, sources)

# Aggregates
make lint
make unit
```

`make ext-test` starts a throwaway Postgres with `shared_preload_libraries =
'axiom'` and an endpoint nothing listens on, so you will see the worker logging
`ping failed` lines in the test output. That is expected: one of the tests
asserts the worker keeps running through failures instead of exiting.

Run the gateway binary directly if you want to see it refuse to start without TLS:

```sh
cd gateway && go run ./cmd/gateway -listen 127.0.0.1:8443
# gateway: tlsconfig: certificate and key paths are both required
```

## 6. Configuring the extension outside compose

**Foreign server and tables** are ordinary FDW DDL. Server options: `endpoint`
(required, `https://` only), `ca_cert` (PEM path; default is the webpki root
store), `rpc_timeout_secs` (default 30). Table options: `resource` (`pods`).
Columns are matched by name from `name`, `namespace`, `phase`, `node` (all
`text`) and `raw` (`jsonb`); you may declare any subset. Failures surface as
SQL errors with FDW SQLSTATEs, e.g. `HV00N` (`fdw_unable_to_establish_connection`)
when the gateway is unreachable, which PL/pgSQL can catch by name.

**The background worker** only starts when the library is preloaded. `CREATE
EXTENSION` alone installs the SQL objects and emits a WARNING telling you the
worker is not running, rather than silently doing nothing.

```
# postgresql.conf
shared_preload_libraries = 'axiom'
axiom.gateway_endpoint   = 'https://gateway:8443'   # https only; embedded user:pw@ is rejected
axiom.gateway_ca_cert    = '/certs/ca.crt'          # optional; default is the Mozilla webpki root store
axiom.ping_interval_secs = 10                        # must exceed rpc_timeout_secs
axiom.rpc_timeout_secs   = 5
```

All four are `SIGHUP`-reloadable. Log lines use a stable prefix so they are easy
to alert on: `axiom bgworker: ping ok ...` at `LOG`, `axiom bgworker: ping failed
...` at `WARNING`.

## 7. Troubleshooting

- **`make ext-test` on macOS fails with "Unix-domain socket path ... is too long"**:
  pgrx puts the scratch cluster under the cargo target directory. Use a short one:
  `CARGO_TARGET_DIR=~/.cache/axiom-target make ext-test PG=pg16`.
- **`make ext-test` says "could not access the server configuration file"**: a
  stale scratch cluster. The target already wipes it, but if you run `cargo pgrx
  test` by hand, delete `extension/target/test-pgdata` first.
- **E2E times out waiting for `ping ok`**: run with `E2E_KEEP=1`, then check
  `docker compose -f deploy/compose/docker-compose.yml logs gateway` for
  `gateway listening` and the postgres log for `axiom bgworker: ping failed`
  lines, which name the gRPC status code.
- **`WARNING: axiom: not loaded via shared_preload_libraries`** after `CREATE
  EXTENSION`: expected if you did not preload the library; add it to
  `shared_preload_libraries` and restart Postgres.

## 8. Repository layout

```
proto/            axiom.v1 protobuf + buf config (Go stubs → gateway/gen, Rust stubs via build.rs)
gateway/          Go gateway: cmd/gateway, internal/server (RPC handlers), internal/k8s (client-go behind an interface), internal/tlsconfig
extension/        pgrx crate: src/{fdw,bgworker,client}.rs (Postgres/network glue),
                  src/{options,quals,pods,transport,config,backoff,ping}.rs (pure, unit-tested)
deploy/compose/   docker-compose.yml + cert generator for the local stack
e2e/              *_test.sh scripts (one per PLAN.md gate) + lib/{stack,kind}.sh shared setup + fixtures/
deploy/k8s/       least-privilege RBAC for the gateway ServiceAccount
.github/          CI: proto drift, gateway, extension, gitleaks, e2e-ping, e2e-pods as separate jobs
docs/             DESIGN.md, PLAN.md, RULES.md
```

## Contributing

Every change must keep the gates in [docs/RULES.md](docs/RULES.md) green: `make
lint unit` locally, and CI runs the same plus `make ext-audit`, gitleaks, and
the E2E for every phase completed so far. New behaviour ships with tests for its
failure paths, not only the happy path.

## License

Apache-2.0. See [LICENSE](LICENSE).
