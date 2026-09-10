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

## Current status: Phase 0 (skeleton and plumbing)

Phase 0 proves the Postgres ↔ gateway boundary works before any Kubernetes code
exists. There is no `kind` cluster, no foreign table, and no `client-go` yet.
What exists and is tested end to end:

| Component | Path | What it does today |
|---|---|---|
| Protobuf API | [proto/axiom/v1/axiom.proto](proto/axiom/v1/axiom.proto) | `GatewayService.Ping` only |
| Gateway (Go) | [gateway/](gateway/) | TLS-only gRPC server that answers `Ping`. There is deliberately no plaintext mode |
| Extension (Rust, pgrx) | [extension/](extension/) | `axiom.*` settings plus a background worker that pings the gateway over TLS on a timer and logs the outcome |
| Local stack | [deploy/compose/](deploy/compose/) | One `docker compose up` brings up gateway + Postgres with a throwaway CA |
| E2E test | [e2e/ping_test.sh](e2e/ping_test.sh) | Starts the stack via [e2e/lib/stack.sh](e2e/lib/stack.sh), runs `CREATE EXTENSION axiom`, asserts a `ping ok` round-trip and TLS 1.3 |

The rest of this README walks you through building, running, and testing it.

## 1. Prerequisites

You need Docker for the quickest path (section 3). For building and unit-testing
the components natively you also need the Go and Rust toolchains.

### Quick path (E2E only)

- Docker Desktop or Docker Engine with Compose v2 (`docker compose version`)
- `bash`, `git`, `make`

### Full developer setup

**Go side**

```sh
# Go 1.25+  (https://go.dev/dl)
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
git checkout phase-0      # until PR #3 is merged into main
```

## 3. Run Phase 0 end to end (the thing to try first)

This is fully automated and needs only Docker:

```sh
make e2e-ping        # alias: make e2e-phase0
```

The first run builds two images and takes several minutes (it compiles
`cargo-pgrx` and the extension inside Docker). Subsequent runs reuse build
caches and take about a minute. You should see:

```
==> building and starting stack
==> CREATE EXTENSION axiom
axiom_version() = 0.1.0
==> asserting the worker reads the configured gateway endpoint
==> asserting background worker is registered
==> waiting up to 90s for a successful Ping round-trip
postgres-1  | ... LOG:  axiom bgworker: ping ok endpoint=https://gateway:8443 gateway_version=dev
==> asserting the gateway serves TLS 1.3 with the generated CA
==> PING E2E PASSED
==> tearing down
```

Knobs: `E2E_TIMEOUT_SECS=120` to wait longer on a slow machine, `E2E_KEEP=1` to
leave the stack running afterwards so you can poke at it (see next section),
`E2E_NO_BUILD=1` to reuse already-built images.

## 4. Poke at the running stack by hand

Bring the stack up and leave it running:

```sh
make up          # or: E2E_KEEP=1 make e2e-ping
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
```

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

Tear everything down (this also deletes the generated certificates):

```sh
make down
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

The background worker only starts when the library is preloaded. `CREATE
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
gateway/          Go gateway: cmd/gateway, internal/server (RPC handlers), internal/tlsconfig
extension/        pgrx crate: src/bgworker.rs (Postgres glue), src/{config,backoff,ping}.rs (pure, unit-tested)
deploy/compose/   docker-compose.yml + cert generator for the local stack
e2e/              *_test.sh scripts (one per PLAN.md gate) + lib/stack.sh shared setup
.github/          CI: proto drift, gateway, extension, gitleaks, e2e-ping as separate jobs
docs/             DESIGN.md, PLAN.md, RULES.md
```

## Contributing

Every change must keep the gates in [docs/RULES.md](docs/RULES.md) green: `make
lint unit` locally, and CI runs the same plus `make ext-audit`, gitleaks, and
the E2E for every phase completed so far. New behaviour ships with tests for its
failure paths, not only the happy path.

## License

Apache-2.0. See [LICENSE](LICENSE).
