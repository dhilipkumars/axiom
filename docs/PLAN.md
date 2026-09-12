# Axiom — Implementation Plan

Companion to [DESIGN.md](./DESIGN.md) and [RULES.md](./RULES.md). Every phase below
is gated by RULES.md — quality, testability, security, and E2E-hardening
requirements apply to each phase's own code as it lands, not as later cleanup.
Breaks the architecture into phases, each
ending in a working, demoable slice with its own E2E test. Phases are ordered so
each one is buildable on real infrastructure (a local `kind` cluster) without
depending on a piece from a later phase.

Test infra assumed throughout: a `kind` cluster spun up in CI, a local Postgres
(via `pgrx run` / a container), and a `justfile`/`Makefile` target `e2e-phaseN` that
brings both up, runs the phase's test, tears down. Each phase's E2E test is additive
— later phases re-run earlier phases' tests as regression checks, not replace them.

---

## Phase 0 — Skeleton & plumbing (no k8s yet)

**Goal**: prove the gRPC-boundary shape works before any Kubernetes code exists.

Tasks:
- [x] Scaffold pgrx extension crate (`cargo pgrx new axiom`), confirm it loads in a
  local Postgres (`CREATE EXTENSION axiom;`).
- [x] Scaffold Go gateway module, minimal `grpc-go` server with a `Ping` RPC.
- [x] Define `.proto` for `Ping` only; generate Rust (`tonic-build`) and Go stubs.
- [x] bgworker skeleton: starts on `_PG_init`, owns a tokio runtime, connects to the
  gateway's `Ping` RPC on a timer, logs success/failure via `elog`.
- [x] Docker Compose / `kind`-free local dev setup: gateway binary + Postgres +
  extension, one `docker-compose up` brings up both.

Notes from implementation: the Postgres↔gateway hop is TLS from Phase 0 (RULES.md
§3); the gateway has no plaintext mode. The bgworker takes a database-less backend
connection so it is visible in `pg_stat_activity` and ready for Phase 3's `NOTIFY`.
Static bgworker registration requires `shared_preload_libraries = 'axiom'`;
`CREATE EXTENSION` alone warns loudly instead of silently running without a worker.
The E2E lives in `e2e/ping_test.sh` (`make e2e-ping`, aliased as `make e2e-phase0`) on
top of the reusable compose setup library `e2e/lib/stack.sh`, which later phases share.

**E2E test (`e2e-phase0`)**: start gateway + Postgres via compose, `CREATE EXTENSION
axiom`, assert the bgworker's log shows a successful `Ping` round-trip within N
seconds. Fully automated, no real cluster involved.

---

## Phase 1 — Read-only, single built-in resource, on-demand only

**Goal**: first real SQL query returns real Kubernetes data. No caching, no watch,
no writes yet — prove the FDW scan → unary RPC → k8s API path end to end.

Tasks:
- [x] Gateway: `List(gvk, namespace_filter)` and `Get(gvk, namespace, name)` RPCs,
  backed by `client-go` against a real cluster (start with **Pods** only).
- [x] Extension: `GetForeignRelSize`/`GetForeignPaths`/`IterateForeignScan` for a
  hardcoded `k8s_pods` foreign table (typed columns: name, namespace, phase, node,
  raw jsonb).
- [x] Qual pushdown: `namespace = X` and `name = Y` translated to RPC filters (not
  fetch-all-then-filter-in-Postgres).
- [x] `CREATE SERVER`/`CREATE FOREIGN TABLE` DDL for `k8s_pods` against one gateway
  endpoint.
- [x] Basic error surfacing: gateway unreachable / RPC error → SQL error, not a
  crash or silent empty result.

Notes from implementation: `List` with a name filter is served by the gateway as a
point `Get` (cheaper for the API server than a field-selector LIST, and a miss is an
empty list). The FDW re-derives the pushed-down filter from `plan.qual` at
`BeginForeignScan` rather than serialising it through `fdw_private`, which keeps one
code path across pg14–17 node layouts. All quals stay local so Postgres re-checks
them; pushdown only narrows the fetch. Literals that cannot be Kubernetes names
(`WHERE name = 'Foo'`) short-circuit to zero rows without an RPC instead of becoming
a gateway `INVALID_ARGUMENT` error. The gateway runs under a pods-`get`/`list`-only
ServiceAccount (`deploy/k8s/gateway-rbac.yaml`); the E2E asserts it cannot read
secrets or delete pods. The E2E is `e2e/pods_test.sh` (`make e2e-pods`, aliased as
`make e2e-phase1`) on `e2e/lib/stack.sh` + `e2e/lib/kind.sh`.

**E2E test (`e2e-phase1`)**: `kind` cluster in CI, apply a few known Pods, run
`SELECT name, phase FROM k8s_pods WHERE namespace = 'default'` from Postgres,
assert the result matches `kubectl get pods -n default`. Also test the point-get
path (`WHERE namespace = 'x' AND name = 'y'`) and a "pod doesn't exist" case.

---

## Phase 2 — Write path

**Goal**: SQL DML actually mutates the cluster, with real conflict handling.

Tasks:
- [x] Gateway: `Create`/`Update`/`Delete` RPCs against the API server, `Update`
  requires a `resourceVersion` and surfaces 409s distinctly from other errors.
- [x] Extension: `ExecForeignInsert`/`ExecForeignUpdate`/`ExecForeignDelete` for
  `k8s_pods` (or switch the demo resource to **ConfigMaps**, cheaper/safer to
  mutate in a shared test cluster than Pods).
- [x] Map a k8s 409 Conflict to a distinct SQL error (not a generic failure) —
  e.g. `SQLSTATE` chosen for "serialization/concurrency conflict."
  the caller can catch/retry.
- [x] Guardrail: writes are never cache-served (already true by design, add a test
  asserting the write path always calls the gateway even if a cache exists later).

Notes from implementation: the demo resource is **ConfigMaps** (`resource
'configmaps'`, columns `name, namespace, data jsonb, raw jsonb`); Pods stay
read-only (`IsForeignRelUpdatable` returns 0). UPDATE/DELETE carry the row's
`raw` column as a resjunk target, which supplies the identity and the
`resourceVersion` read by the scan; the gateway sends it as the PUT precondition
and maps a 409 to gRPC `ABORTED`, which the extension raises as SQLSTATE `40001`
(`serialization_failure`). A stale write is therefore retryable with the same
idiom as any Postgres serialization failure. Writes bypass everything but the
gateway RPC by construction; the E2E asserts every DML shows up in the gateway's
request log. Kubernetes has no transactions, so a write is durable at statement
execution and not undone by ROLLBACK (DESIGN.md §2 non-goal). The E2E lives in
`e2e/configmaps_test.sh` (`make e2e-configmaps`, alias `make e2e-phase2`).

**E2E test (`e2e-phase2`)**: `INSERT INTO k8s_configmaps (...)` from Postgres, assert
`kubectl get configmap` shows it; `UPDATE ... SET data = ...`, assert the cluster
reflects the change; concurrently update the same object out-of-band via `kubectl`
between a Postgres `SELECT` and `UPDATE` to force a 409, assert Postgres raises the
distinct conflict error rather than silently overwriting or crashing; `DELETE`,
assert it's gone cluster-side.

---

## Phase 3 — Watch-driven cache (the "live" tier)

**Goal**: the actual differentiator — standing watch, shared-memory cache, no RPC
per read.

Tasks:
- [x] Gateway: `Subscribe(gvk, namespace_filter, resourceVersion?) ->
  stream<WatchEvent>` server-streaming RPC, supporting resync-from-bookmark.
  **Scope change (approved in review of PR #7):** implemented as a raw
  `client-go` list+watch per stream rather than an informer. An informer's own
  store would duplicate the extension's cache and its resync semantics hide the
  resourceVersion bookkeeping the extension needs for honest tiers. Sharing one
  upstream watch across subscribers to the same `(gvk, namespace)` (the
  informer-factory fan-out concern, DESIGN.md §8) is Phase 5's job.
- [x] Extension bgworker: opens one persistent `Subscribe` stream per configured
  cluster+GVK, reconnect/backoff on drop, relist on resume.
- [x] Shared-memory cache (`dshash`) keyed `(cluster_id, gvk, namespace, name)`,
  populated by the bgworker from stream events; tombstone-and-sweep for deletes.
- [x] Subscription/consistency-tier state tracked per `(cluster_id, gvk,
  namespace_filter)`: `ACTIVE` / `RESYNCING` / `DEGRADED`.
- [x] `IterateForeignScan` updated to serve from cache when `ACTIVE`, fall through
  to direct RPC when no subscription exists yet (`cache_mode 'on_demand'` default
  for tables not yet touched), and surface staleness when `DEGRADED` (a
  `k8s_watch_status('k8s_pods')` helper function, or a hidden `_stale_since`
  column).
- [x] `LISTEN`/`NOTIFY` emitted by the bgworker on cache changes.

Notes from implementation: the cache is a **DSA-backed hash index managed by the
extension** rather than `dshash`: pgrx exposes DSA but not dshash, and dshash's
parameter struct changed layout in pg17; one `LWLock` guards the whole cache,
which is adequate here (partitioned locking is revisited in Phase 5 once
contention is measurable). Subscriptions are **requested by scans**: the first
scan of a `cache_mode 'watch'` table registers `(server, kind, namespace)` in a
shared slot table and is served on demand; the background worker opens one
`Subscribe` stream per slot, resumes from the stored bookmark after a loss, and
relists only on `RESYNC_REQUIRED`. Staleness is never masked: a `DEGRADED`
subscription is still served, with a `WARNING` on every scan, and
`axiom_watch_status()` exposes state, object count, bookmark, ages, and reason. A
resumed stream stays `DEGRADED` until the API server's first `BOOKMARK` (which it
only sends to a caught-up watcher) proves the backlog is delivered; there is no
other honest "current again" signal on resume. Initial-listing events carry no
resume point, so a stream dropped mid-listing relists rather than resuming from a
partial cache.
`NOTIFY axiom_events` carries a JSON payload `{server, resource, namespace, name,
type}`; the worker connects to `axiom.notify_database` to send it. Cache memory is
bounded by `axiom.cache_size_mb`; when exhausted the affected subscription becomes
`DEGRADED` instead of evicting (eviction policy remains an open item, DESIGN.md §8).
The in-process stub gateway test drives the full lifecycle without a cluster:
warm → ACTIVE → live events → DEGRADED (stale serve) → resume with replay, no relist.
The E2E lives in `e2e/watch_test.sh` (`make e2e-watch`, alias `make e2e-phase3`).

**E2E test (`e2e-phase3`)**: run a `SELECT` to warm the watch on `k8s_pods`, then
create/delete Pods via `kubectl` directly (not through Postgres) and assert a
subsequent `SELECT` from Postgres reflects the change within a bounded latency
**without** the extension issuing a new List RPC (assert via a gateway-side call
counter or metric that only one initial List happened). Separate test: kill the
gateway pod mid-stream, assert the extension marks the subscription `DEGRADED`,
restart the gateway, assert it resyncs and returns to `ACTIVE` with correct state
(no missed/duplicated events versus a `kubectl` ground truth). Separate test:
`LISTEN k8s_events; ...` in a psql session, mutate via `kubectl`, assert a
`NOTIFY` payload arrives with the right GVK/name/type.

---

## Phase 4 — CRDs & schema discovery

**Goal**: generalize beyond hardcoded built-ins to arbitrary CRDs.

Tasks:
- [x] Gateway: `DiscoverSchema(gvk) -> ColumnSchema` using the cluster's
  discovery/OpenAPI client.
- [x] Extension: generic scan/DML path driven by discovered schema instead of
  hardcoded Rust structs (promoted scalar columns + `spec jsonb`/`status jsonb`
  catch-all, per DESIGN.md §5.4).
- [x] `IMPORT FOREIGN SCHEMA` implementation: queries `DiscoverSchema` for all (or a
  filtered set of) GVKs in a cluster and emits `CREATE FOREIGN TABLE` statements.
- [x] Extend watch/cache machinery (already generic by `gvk` key from Phase 3) to
  a test CRD — should require no changes if Phase 3 was built generically; this
  phase is partly a regression check on that assumption.

Notes from implementation: replacing the gateway's static two-kind registry with
discovery would have widened it to everything in the cluster, so the served set
became an explicit deployment decision: a **`--serve` allowlist** of
`plural[.group]` entries (default `pods,configmaps`, exactly what the registry
held), with the ServiceAccount's RBAC scoped to match. A kind outside the
allowlist and a kind the cluster does not have return the *same* error, so the
allowlist cannot be enumerated by probing. Resolution caches per group-version
and invalidates once on a miss, so a CRD created while the gateway runs resolves
without a restart.

**Two RPCs, not one:** `DiscoverSchema` for a single kind and `ListKinds` for
enumeration, because `IMPORT FOREIGN SCHEMA` needs every kind's shape in one
round-trip rather than one call per table.

The extension's closed `Kind` enum became a runtime `Resource`
(`group/version/kind/plural` + scope), kept `Copy` and plain-data with inline
bounded strings so it drops into a shared-memory subscription slot unchanged.
**Discovery never happens on the scan path**: `IMPORT` writes the resolved
identity into each table's options, and column *meaning* comes from the column
name alone via a projection rule that mirrors the gateway's column rule
(`extension/src/schema.rs` against `gateway/internal/k8s/schema.go`, with the
same normalization cases asserted on both sides). A declared column matching no
promoted column is a top-level lookup that reads NULL when the kind lacks the
field — a deliberate change from Phases 1-3, which rejected unknown names; a
CRD's fields are not knowable without discovery and a scan must not discover.
Column *types* stay strictly checked, which is what still catches real mistakes.

Generated DDL quotes and strictly validates every identifier, because a CRD's
group/kind/field names are attacker-influenceable in a multi-tenant cluster and
arrive across the gRPC boundary (RULES.md §3); a name that fails validation
drops its column or its table rather than being sanitised into a possible
collision. One unusable CRD warns and is skipped rather than failing the whole
import. `cache_mode 'watch'` is emitted only for kinds the API server says it
will watch.

Phase 3's cache needed no structural change, which was the regression check this
phase was partly there to perform. The test CRD deliberately has **no status
subresource**: with one, the API server silently ignores status changes in a PUT
to the main resource, so a SQL `UPDATE` of `status` would appear to succeed and
change nothing. Writing a status subresource needs its own request and is not in
Phase 4's scope. The E2E lives in `e2e/crd_test.sh` (`make e2e-crd`, alias
`make e2e-phase4`).

**E2E test (`e2e-phase4`)**: apply a test CRD + CRD instances to the `kind` cluster,
run `IMPORT FOREIGN SCHEMA` against it, assert the generated foreign table's columns
match the CRD schema, `SELECT`/`INSERT`/`UPDATE`/watch against it exactly as in
Phases 1–3's tests but parameterized over the CRD instead of Pods/ConfigMaps —
i.e. re-run the Phase 1–3 test suite generically against a CRD to prove genericity.

---

## Phase 5 — Multi-cluster

**Goal**: prove the `CREATE SERVER`-per-cluster abstraction actually holds up with
≥2 independent clusters/gateways simultaneously.

Tasks:
- [ ] bgworker: manage N independent persistent streams (one per configured
  server), independent reconnect/backoff state per cluster.
- [ ] Cache key already includes `cluster_id` (from Phase 3) — this phase is mostly
  a concurrency/isolation test, plus config surface (`CREATE SERVER ... OPTIONS
  (endpoint ...)` per cluster wired end to end).
- [ ] Verify a failure/degradation in one cluster's stream doesn't affect another
  cluster's `ACTIVE` state or block its scans (isolation, not just "it also works").

**E2E test (`e2e-phase5`)**: two `kind` clusters in CI, two gateways, two `CREATE
SERVER`s. Query both, mutate one cluster's data, assert only that cluster's cached
table updates. Kill one gateway, assert the other cluster's `ACTIVE`/live queries
are unaffected (isolation test), then bring it back and assert it resyncs.

---

## Phase 6 — Gateway auth hardening

**Goal**: replace the POC's placeholder auth with the real model from DESIGN.md §7.

Tasks:
- [ ] mTLS between extension gRPC client and gateway.
- [ ] `CREATE USER MAPPING` credential → gateway-side mapping to a scoped k8s RBAC
  identity (not a shared superuser token).
- [ ] Negative tests: a Postgres role mapped to a restricted identity cannot read/
  write resources outside its RBAC scope, even though the underlying gateway
  connection is shared infrastructure.

**E2E test (`e2e-phase6`)**: two Postgres roles with two different `CREATE USER
MAPPING`s pointing at two different RBAC-scoped identities on the same cluster;
assert role A can read/write only what its RBAC identity permits, role B is
correctly denied (SQL error, not a crash) for out-of-scope resources; assert
plaintext/non-mTLS connections to the gateway are rejected.

---

## Cross-cutting, applies to every phase

- Every phase's E2E test must run unattended in CI against a real `kind` cluster —
  no phase is "done" on the strength of manual testing alone.
- Each new phase's CI job re-runs all previous phases' E2E tests as regressions
  before declaring the new phase's own test authoritative.
- Track the open risks from DESIGN.md §8 (CRD schema explosion, watch/informer
  scaling, shared-memory eviction policy) as they become concretely testable —
  Phase 4 is the natural point to revisit CRD schema explosion, Phase 5 for
  informer scaling, and cache eviction should get its own task once a phase
  exercises a high-object-count cluster (not yet scheduled — flag if that becomes a
  near-term need rather than scheduling it speculatively now).
