# Developer guide

How to build, run and test Axiom. To *use* Axiom, start at the
[documentation site](https://dhilipkumars.github.io/axiom/) instead — the two
barely overlap, and this page will not tell you how to write a query.

The engineering documents are the other half of this:

| Document | What it covers |
|---|---|
| [DESIGN.md](DESIGN.md) | Architecture, why a gateway, consistency tiers, schema mapping, multi-cluster, auth |
| [RULES.md](RULES.md) | The gates every change must meet: quality, testability, security, E2E |
| [AUTH.md](AUTH.md) | Per-caller identity: the options, the threat model, the recommended flow |
| [PLAN.md](PLAN.md) | The development phases, each with its own E2E gate |
| [RELEASING.md](RELEASING.md) | Cutting a release, and what each version number means |

## 1. Prerequisites

You need Docker for the quickest path (section 3). For building and unit-testing
the components natively you also need the Go and Rust toolchains.

### Quick path (E2E only)

- Docker Desktop or Docker Engine with Compose v2 (`docker compose version`)
- `bash`, `git`, `make`
- For every gate but Phase 0: [`kind`](https://kind.sigs.k8s.io) and `kubectl`

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
# Rust 1.96 or newer via rustup (https://rustup.rs); cargo-pgrx 0.19 requires it
rustc --version

# PostgreSQL 16, 17 or 18 with server headers. Examples:
#   macOS:   brew install postgresql@16
#   Debian:  apt install postgresql-16 postgresql-server-dev-16
# pgrx also needs clang/libclang for bindgen (brew install llvm / apt install clang libclang-dev)

cargo install cargo-pgrx --version 0.19.2 --locked
cargo install cargo-audit cargo-deny --locked

# Tell pgrx which Postgres to use. The flag must name the same major as the
# pg_config it is given, so change both together:
cargo pgrx init --pg16 /opt/homebrew/opt/postgresql@16/bin/pg_config   # macOS
cargo pgrx init --pg18 /usr/lib/postgresql/18/bin/pg_config            # Debian, pg18
```

Check what pgrx knows about with `cat ~/.pgrx/config.toml`. The `make` targets
below take `PG=pg16` (or `pg17`, `pg18`) to match.

**Supported Postgres versions are 16 through the latest major.** 14 and 15 were
dropped: 14 reaches end of life in November 2026, and neither is where the
installed base sits. 16 is supported upstream until November 2028. 19 is still
in beta and is not built here yet.

## Against a cluster you already have

One command from a kind cluster to a Postgres that answers questions about it:

```sh
make local-dev-up CONTEXT=kind-mycluster   # or as an environment variable
make local-dev-up                          # or just: current-context
make local-dev-psql
make local-dev-down
```

It mints a throwaway CA and certificate, starts the gateway against that
cluster, starts Postgres with the extension, imports the schema, and prints the
`psql` command to connect. `kubectl` is untouched.

**What is supported**

| | |
|---|---|
| **kind** | yes — macOS and Linux |
| docker-desktop, k3d, minikube | refused with a message; tracked in [#68](https://github.com/dhilipkumars/axiom/issues/68) |
| EKS, GKE, AKS | refused — see below |

**Why kind works the same on both platforms.** A kind kubeconfig names the API
server as `127.0.0.1:<port>`, which inside a container is the container. Rather
than rewrite that address, the gateway joins kind's own Docker network and uses
`kind get kubeconfig --internal`, which addresses the node container directly.
That is the route `e2e/lib/kind.sh` takes and CI exercises on every PR. Postgres
stays on the compose network, so nothing binds a host port your own Postgres
might hold.

**Why a cloud kubeconfig is refused.** EKS, GKE and AKS kubeconfigs carry an
`exec:` block rather than a credential — *"run this program to get a token"*.
**client-go** runs it, not `kubectl`, so the requirement travels with the file
into the gateway's distroless image, which has no shell and no cloud CLI. This
is not platform-specific. For a remote cluster, give the gateway a
ServiceAccount token instead:

```sh
kubectl --context=<ctx> apply -f ../deploy/k8s/gateway-rbac.yaml
kubectl --context=<ctx> -n axiom-system create token axiom-gateway --duration=8h
```

**Re-running is how you pick up a newly granted kind.** The gateway reads its
kubeconfig once and caches authorization decisions for its lifetime, so
`local-dev-up` recreates it rather than leaving it running. It also replaces the
imported foreign tables — only those, and without `CASCADE`, so if a view of
yours depends on one it stops and tells you rather than dropping your work.

## 2. Get the code

```sh
git clone https://github.com/dhilipkumars/axiom.git
cd axiom
```

## 3. Run the E2E gates (the thing to try first)

Both are fully automated. Phase 0 needs only Docker:

```sh
make e2e-ping        # Phase 0 gate (alias: make e2e-phase0), Docker only
make e2e-pods        # Phase 1 gate (alias: make e2e-phase1), needs kind + kubectl
make e2e-configmaps  # Phase 2 gate (alias: make e2e-phase2), needs kind + kubectl
make e2e-watch       # Phase 3 gate (alias: make e2e-phase3), needs kind + kubectl
make e2e-crd         # Phase 4 gate (alias: make e2e-phase4), needs kind + kubectl
make e2e-cluster     # Phase 5 gate (alias: make e2e-phase5), needs kind + kubectl
make e2e             # all gates, oldest first, sharing one build and one cluster
```

`make e2e` runs every gate through [e2e/run_all.sh](https://github.com/dhilipkumars/axiom/blob/main/e2e/run_all.sh), which
builds the images once and creates one kind cluster for the whole suite, then
gives each gate a fresh compose stack. The individual targets above still stand
alone; the driver exists because the image build dominates everything else.
It prints a per-gate timing summary at the end, and takes a subset as
arguments: `./e2e/run_all.sh watch crd`.

Every gate but Phase 0 needs `kind` and `kubectl`. Each creates a cluster named
`axiom-e2e`, applies the least-privilege RBAC and its own fixtures, and deletes
the cluster afterwards.

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

Bring up a stack and leave it running. The Phase 4 harness gives you the most to
look at: a kind cluster, the test CRD with a couple of Widgets, and an already
imported `k8s` schema.

```sh
E2E_KEEP=1 E2E_KIND_KEEP=1 make e2e-crd     # cluster + CRD + imported tables
E2E_KEEP=1 E2E_KIND_KEEP=1 make e2e-pods    # cluster + pods only
make up                                      # no cluster at all (Ping only)
```

The test deletes its own Widgets on the way out, so put them back:

```sh
export KUBECONFIG="$PWD/e2e/.kind/admin"
kubectl apply -f e2e/fixtures/widgets.yaml
kubectl -n axiom-e2e get widgets            # your oracle for comparing against SQL
```

### Talking to this stack with `docker compose`

**Always pass the same `-f` files you started with.** The base compose file
runs the gateway with `-no-cluster`; the kind overlay is what replaces that with
a kubeconfig. Any `up` or `restart` that omits the overlay will quietly
reconfigure the gateway to serve no cluster, and every query then fails with
`gateway has no cluster credentials configured`. Set this once per shell:

```sh
export E2E_KUBE_DIR="$PWD/e2e/.kind"
ac() { docker compose -f deploy/compose/docker-compose.yml \
                     -f deploy/compose/docker-compose.kind.yml "$@"; }
```

`ac` stands in for that pair below. A function rather than an alias or a
variable, because it behaves the same in bash and zsh and inside scripts.

`make up` and `make down` deliberately use only the base file: `up` is the
no-cluster Ping stack, and `down` removes containers regardless of overlays.

### Connecting to Postgres

Port 5432 is **not published to the host** — the compose file keeps it inside
the compose network. The session that always works:

```sh
ac exec postgres psql -U axiom -d axiom
```

For a GUI client or a local `psql`, publish the port with a third overlay:

```sh
cat > /tmp/expose-pg.yml <<'YAML'
services:
  postgres:
    ports:
      - "55432:5432"
YAML
ac -f /tmp/expose-pg.yml up -d

PGPASSWORD=axiom-dev psql -h 127.0.0.1 -p 55432 -U axiom -d axiom
```

Credentials are `axiom` / `axiom-dev`, database `axiom`, no SSL. Keep the
overlay out of `deploy/compose/`: a file named `docker-compose.override.yml`
there would be loaded automatically and would publish the port during E2E runs
too. Note that recreating the Postgres container empties the shared-memory
watch cache, so any `cache_mode 'watch'` table starts cold again.

### Things worth trying

```sql
-- Install the extension's SQL objects (idempotent)
CREATE EXTENSION IF NOT EXISTS axiom;
SELECT axiom_version();

-- The settings the background worker is using
SHOW axiom.gateway_endpoint;
SHOW axiom.ping_interval_secs;

-- The worker is a real Postgres process, visible like any backend
SELECT pid, backend_type, backend_start FROM pg_stat_activity
 WHERE backend_type = 'axiom gateway pinger';
```

**Discovery and import.** The e2e already created the server and a `k8s`
schema; this is how to do it yourself, and how to scope an import to one API
group:

```sql
CREATE SERVER kind FOREIGN DATA WRAPPER axiom_fdw
  OPTIONS (endpoint 'https://gateway:8443', ca_cert '/certs/ca.crt', rpc_timeout_secs '10');

CREATE SCHEMA crds;
IMPORT FOREIGN SCHEMA "example.com" FROM SERVER kind INTO crds;   -- just the CRD group
\d crds.widgets                                                   -- columns discovery chose

-- The generated DDL carries the resolved identity, which is why no scan needs discovery
SELECT ftoptions FROM pg_foreign_table ft
  JOIN pg_class c ON c.oid = ft.ftrelid WHERE c.relname = 'widgets';
```

**Read a CRD through the generic projection.** No Rust knows what a Widget is;
`spec` and `status` are top-level fields matched by column name:

```sql
SELECT name, spec->>'size', spec->>'color', status->>'phase', labels
  FROM k8s.widgets WHERE namespace = 'axiom-e2e' ORDER BY name;

EXPLAIN SELECT name FROM k8s.widgets WHERE namespace = 'axiom-e2e';  -- plans without contacting the gateway
```

**Write to it**, and watch the guardrails:

```sql
INSERT INTO k8s.widgets (name, namespace, spec)
  VALUES ('manual', 'axiom-e2e', '{"size":1,"color":"teal"}');
UPDATE k8s.widgets SET spec = spec || '{"color":"pink"}' WHERE name = 'manual';

UPDATE k8s.widgets SET uid = 'forged' WHERE name = 'manual';   -- 0A000: server-managed
UPDATE k8s.widgets SET name = 'renamed' WHERE name = 'manual'; -- 0A000: identity is immutable
DELETE FROM k8s.widgets WHERE name = 'manual';
```

Built-in kinds need no `group`/`version`/`kind`, so the Phase 1-3 spellings
still work unchanged:

```sql
CREATE FOREIGN TABLE k8s_pods (name text, namespace text, phase text, node text, raw jsonb)
  SERVER kind OPTIONS (resource 'pods');
CREATE FOREIGN TABLE k8s_configmaps (name text, namespace text, data jsonb, raw jsonb)
  SERVER kind OPTIONS (resource 'configmaps');

SELECT name, phase, node FROM k8s_pods WHERE namespace = 'kube-system';
INSERT INTO k8s_configmaps (name, namespace, data) VALUES ('app', 'default', '{"LOG_LEVEL":"info"}');
UPDATE k8s_configmaps SET data = data || '{"LOG_LEVEL":"debug"}' WHERE namespace = 'default' AND name = 'app';
DELETE FROM k8s_configmaps WHERE namespace = 'default' AND name = 'app';
```

Watch the gateway log to see pushdown in action: each `List` logs the namespace
and name filters it received:

```sh
ac logs -f gateway | grep '"msg":"list"'
```

**The allowlist is not discovery.** The gateway only offers what `--serve`
names, which the kind overlay sets to `pods,configmaps,widgets.example.com`.
Create a CRD outside that list and it stays invisible, and a hand-written table
for it fails at scan with the same error as a kind that does not exist:

```sh
kubectl apply -f - <<'YAML'
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata: {name: gizmos.example.com}
spec:
  group: example.com
  scope: Namespaced
  names: {plural: gizmos, singular: gizmo, kind: Gizmo, listKind: GizmoList}
  versions: [{name: v1, served: true, storage: true,
              schema: {openAPIV3Schema: {type: object, properties: {spec: {type: object}}}}}]
YAML
```

```sql
CREATE SCHEMA late;
IMPORT FOREIGN SCHEMA k8s FROM SERVER kind INTO late;   -- widgets yes, gizmos no
```

**Live tables.** Add `cache_mode 'watch'` to serve a table from the
shared-memory cache instead of an RPC per scan. The first scan is served on
demand and starts the watch; once `axiom_watch_status()` shows `ACTIVE`, scans
read the cache and kubectl-side changes appear within watch latency. If the
gateway goes away the subscription turns `DEGRADED` and the cache is still
served, with a `WARNING` on every scan; when it returns the stream resumes from
its bookmark. A resumed stream stays `DEGRADED` until the API server's first
bookmark proves the backlog was delivered, which can take up to a minute.
Change notifications: `LISTEN axiom_events;` in the database named by
`axiom.notify_database` (payload: `{"server","resource","namespace","name","type"}`,
where `resource` is the kubectl spelling, e.g. `widgets.example.com`).

```sql
SELECT count(*) FROM k8s.widgets_live;   -- first scan registers the subscription
SELECT * FROM axiom_watch_status();      -- wait for ACTIVE
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
ac logs -f postgres | grep "axiom bgworker"
```

Now break things and watch it cope. Stop the gateway and you should see
`ping failed ... code=Unavailable` at `WARNING` with the retry delay doubling
from 1s up to 60s; start it again and the next attempt logs `ping ok` and the
interval resets. `stop`/`start` reuse the existing container, so they are safe
without the overlays:

```sh
ac stop gateway
# ...watch the warnings and backoff, and any watch table go DEGRADED...
ac start gateway
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

# Documentation: regenerate the reference pages, and the CI drift check
make docs-generate
make docs-check

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
store), `rpc_timeout_secs` (default 30).

Table options identify the kind. `resource` (the plural name) is always
required, and is enough on its own for the two built-in kinds, `pods` and
`configmaps`. Any other kind also needs `version` and `kind`, plus `group`
unless it is in the core API group. `namespaced` (default `true`), `writable`
and `cache_mode` are optional. `IMPORT FOREIGN SCHEMA` writes all of these for
you, which is the expected way to define a CRD table:

```sql
CREATE FOREIGN TABLE widgets (name text, namespace text, spec jsonb, raw jsonb)
  SERVER kind
  OPTIONS (resource 'widgets', group 'example.com', version 'v1', kind 'Widget');
```

Because the identity lives in the options, no scan or write ever contacts the
gateway for schema. Discovery happens once, during `IMPORT FOREIGN SCHEMA`.

Columns are matched **by name**, and any subset may be declared:

| Column | Type | Reads |
|---|---|---|
| `api_version`, `kind` | `text` | the object's own `apiVersion` and `kind` |
| `metadata` | `jsonb` | the whole `metadata` object |
| `name`, `namespace`, `uid`, `resource_version`, `creation_timestamp` | `text` | the matching `metadata` field |
| `labels`, `annotations` | `jsonb` | the matching `metadata` map |
| `raw` | `jsonb` | the whole object; required for `UPDATE`/`DELETE` |
| `phase`, `node` on `pods` | `text` | `status.phase`, `spec.nodeName` |
| anything else | `jsonb` | the object's top-level field of that name |

`api_version`, `kind` and `metadata` are the only three fields guaranteed to
exist on every Kubernetes object: `spec` is present on about two thirds of
built-in kinds and `status` on under half, so neither is a safe basis for a
query spanning kinds. `metadata` also carries what no individual column
promotes, such as `ownerReferences` and `finalizers`. All three are read-only.

The last row is what makes CRDs work without a per-kind mapping: a column named
`spec` reads `spec`, and a `camelCase` field is reached by its `snake_case`
column name (`string_data` reads `stringData`). A column naming a field the
kind does not have reads NULL rather than being rejected, since a CRD's fields
are not knowable without discovery and a scan deliberately never discovers.
Column *types* are still checked strictly, so a mistyped column fails loudly.
`uid`, `resource_version` and `creation_timestamp` are server-managed: they
read fine but writing them raises an error instead of being quietly dropped.

Failures surface as SQL errors with FDW SQLSTATEs, e.g. `HV00N`
(`fdw_unable_to_establish_connection`) when the gateway is unreachable, which
PL/pgSQL can catch by name.

**`IMPORT FOREIGN SCHEMA`** takes the remote schema name as an API group, since
Kubernetes has no schemas of its own. Two spellings mean "everything this
gateway serves": the literal `k8s`, and **the server's own name** — so
`IMPORT FOREIGN SCHEMA prod FROM SERVER prod INTO prod` reads naturally under
the one-schema-per-cluster model. `core` and `v1` both mean the core group,
whose real name is the empty string and cannot be typed as a schema name.
Anything else is an API group, such as `example.com`. `LIMIT TO` and `EXCEPT` filter by plural
name. Options: `cache_mode` (applied to every generated table the API server
will actually watch) and `prefix` (prepended to each table name, so two
clusters can be imported into one schema).

```sql
IMPORT FOREIGN SCHEMA "example.com" FROM SERVER kind INTO crds;
IMPORT FOREIGN SCHEMA k8s LIMIT TO (pods, configmaps) FROM SERVER kind INTO k8s
  OPTIONS (cache_mode 'watch', prefix 'prod_');
```

A kind whose name cannot be a safe SQL identifier is skipped with a `WARNING`
naming it, rather than failing the whole import; the same applies to individual
columns, which stay reachable through `raw`.

**What a gateway serves is bounded by its own RBAC.** Discovery asks the API
server which kinds the gateway's ServiceAccount may list, via
`SelfSubjectAccessReview`, and offers only those. Scope the ServiceAccount and
the served set follows; there is no second list to keep in step.

Two consequences worth knowing. Kubernetes grants some kinds to every
ServiceAccount through its own default bindings, so a few things appear that
your ClusterRole never mentions — `clustertrustbundles` is bound to the
`system:serviceaccounts` group, for instance. And access answers are cached for
the gateway's lifetime, because an import asks about every kind at once and
RBAC does not change mid-import, so restart the gateway to pick up a changed
ClusterRole.

The `--serve` flag remains as optional narrowing, a comma list of
`plural[.group]` entries where `*.group` covers a group and `*.*` covers
everything. It defaults to `*.*`, meaning "narrow nothing". Use it to hide
kinds the identity could otherwise read. A kind outside it is reported exactly
as a kind the cluster does not have, so it cannot be enumerated by probing.

**Table names** are the plural, disambiguated by group only where two kinds
collide. `events` exists in both the core group and `events.k8s.io`, so a
whole-cluster import yields `events_core` and `events_events_k8s_io` and no
bare `events`. Handing the bare name to one of them would make `events` mean
whichever the rule happened to favour.

**The background worker and the shared cache** both require the library to be
preloaded, and neither can be set up after startup, so `CREATE EXTENSION`
without it fails with an error naming this setting rather than installing
something that cannot work.

```
# postgresql.conf
shared_preload_libraries = 'axiom'
axiom.gateway_endpoint   = 'https://gateway:8443'   # https only; embedded user:pw@ is rejected
axiom.gateway_ca_cert    = '/certs/ca.crt'          # optional; default is the Mozilla webpki root store
axiom.ping_interval_secs = 10                        # must exceed rpc_timeout_secs
axiom.rpc_timeout_secs   = 5
axiom.notify_database    = 'postgres'   # where the worker sends NOTIFY axiom_events
axiom.cache_size_mb      = 256          # bound on the shared-memory watch cache
```

The first four are `SIGHUP`-reloadable; the last two apply at worker start. Log lines use a stable prefix so they are easy
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
- **`ERROR: axiom must be loaded through shared_preload_libraries`** from
  `CREATE EXTENSION`: the library was not preloaded. Add it to
  `shared_preload_libraries` and restart Postgres. Axiom cannot be loaded on
  demand at all -- two of its GUCs are `PGC_POSTMASTER`.
- **`invalid peer certificate: BadSignature`** on a stack that was working:
  the gateway is serving a stale certificate. The `certs` service is a one-shot
  that regenerates the CA and server cert on every `up`, but Compose only
  recreates a container whose configuration changed, so a repeated `up` can
  rewrite the volume while leaving a long-running gateway holding the cert it
  loaded at startup. Restart the gateway so it re-reads them; rebuilding is not
  needed.

  ```sh
  ac restart gateway
  # confirm the gateway started after the certs were written:
  docker inspect axiom-gateway-1 --format '{{.State.StartedAt}}'
  docker inspect axiom-certs-1   --format '{{.State.FinishedAt}}'
  ```
- **`gateway has no cluster credentials configured`** on every query: the
  gateway was recreated without the kind overlay and is running with
  `-no-cluster`. Bring it back up with both `-f` files (see section 4);
  `docker inspect axiom-gateway-1 --format '{{json .Args}}'` shows which
  flags it actually has.

## 8. Repository layout

```
proto/            axiom.v1 protobuf + buf config (Go stubs → gateway/gen, Rust stubs via build.rs)
gateway/          Go gateway: cmd/gateway, internal/server (RPC handlers),
                  internal/k8s (client-go behind an interface, plus discovery/allowlist/column rules),
                  internal/cli (the flag definitions, shared with docsgen), internal/tlsconfig,
                  cmd/docsgen (generates docs/generated/ from proto, flags, options and column tables)
extension/        pgrx crate: src/{fdw,bgworker,client,shmem}.rs (Postgres/network glue),
                  src/{resource,schema,table,import,options,quals,cache,transport,config,backoff,ping}.rs
                  (pure, unit-tested; schema.rs is the column-projection rule paired with the gateway's)
deploy/compose/   docker-compose.yml + cert generator for the local stack
e2e/              *_test.sh scripts (one per PLAN.md gate) + lib/{stack,kind}.sh shared setup + fixtures/ (pods, test CRD)
deploy/k8s/       gateway Deployment + NodePort Service, and least-privilege RBAC for its ServiceAccount
.github/          CI: proto drift, generated-docs drift, gateway, extension, gitleaks, and one job per E2E gate,
                  chained so a later gate implies the earlier ones; plus the GitHub Pages publish
docs/             index.md + guides/ (the site, written by hand), generated/ (produced by make docs-generate),
                  and DESIGN.md, AUTH.md, PLAN.md, RULES.md (engineering documents)
mkdocs.yml        site configuration; docs_dir is docs/ itself, so the site cannot disagree with the tree
.changes/         changeset notes for user-visible changes (see .changes/README.md)
```

## Conventions a change has to follow

Reference pages under `docs/generated/` are produced by `make docs-generate` and
must be committed. CI regenerates them and fails on a difference, the same way
`make proto-check` guards the generated protobuf code. A change to a `.proto`
comment, an FDW option or a column rule therefore requires regenerating; a
refactor that changes none of them does not.

A user-visible change also wants a note under `.changes/`; see
`.changes/README.md` for what counts and what does not. Whether a change needed
one is enforced in review rather than by CI, on purpose; whether a changeset
that exists is well-formed is enforced by `make release-check`.

Releases assemble those notes into [CHANGELOG.md](https://github.com/dhilipkumars/axiom/blob/main/CHANGELOG.md). The version
lives in `extension/Cargo.toml` and everything else derives from it —
[docs/RELEASING.md](RELEASING.md) is the procedure.

## The gates

Every change keeps [RULES.md](RULES.md) green. Locally that is `make lint unit`;
CI runs the same plus `make ext-audit`, `govulncheck`, gitleaks, and the E2E
gates. New behaviour ships with tests for its failure paths, not only the happy
path — a test that only proves the good case passes is the kind this project
has repeatedly found to be worthless.
