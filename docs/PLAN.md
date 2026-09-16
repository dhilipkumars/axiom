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
Static bgworker registration requires `shared_preload_libraries = 'axiom'`, and
so does the shared cache, so `_PG_init` refuses to load without it rather than
running crippled: `CREATE EXTENSION` alone fails with an error naming the
setting.
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
code path across the supported pg16+ node layouts. All quals stay local so Postgres re-checks
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
  informer-factory fan-out concern, DESIGN.md §8) is Phase 6's job.
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
which is adequate here (partitioned locking is revisited in Phase 6 once
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
bounded by `axiom.cache_size_mb`; when exhausted the affected subscription stops
taking objects instead of evicting (eviction policy remains an open item,
DESIGN.md §8) -- `DEGRADED` and served stale if it had already synced,
`REQUESTED` and not served at all if it filled while still building, since a
partial listing cannot know which rows it is missing.
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

## Phase 5 — Data model: a schema per cluster, a table per kind

**Goal**: point Axiom at a cluster and query anything in it, without naming
kinds one at a time. Phase 4 proved discovery works per kind; this phase makes
"everything the gateway may see" the unit of work, and fixes what breaks at
that scale.

The ordering matters: this lands *before* multi-cluster, because the schema
naming and the cluster identity it implies are the same decision, and doing
multi-cluster first would mean choosing them twice.

Tasks:
- [x] **Schema per cluster, table per kind** as the documented default.
  Already expressible today (`IMPORT FOREIGN SCHEMA k8s FROM SERVER prod INTO
  prod`), but the docs and the E2E model it as `INTO k8s`, which reads as if
  the remote-schema argument were the target schema rather than an API-group
  filter. Make the cluster-named form the example everywhere, and accept the
  server's own name as a synonym for "every group this gateway serves".
- [x] **RBAC-bounded discovery**: filter the served set by
  `SelfSubjectAccessReview` against the gateway's own identity, so what is
  offered follows what the ServiceAccount can actually read. `system:basic-user`
  grants `create` on `selfsubjectaccessreviews` to `system:authenticated` by
  default, so this needs no extra permission. Today `--serve` and the
  ServiceAccount's RBAC are two lists that must be hand-synced — a comment in
  `deploy/k8s/gateway-rbac.yaml` says exactly that, which is the tell. After
  this, RBAC is the single source of truth and `--serve` defaults to everything,
  surviving only as optional narrowing for operators who want to hide kinds they
  could otherwise read.
- [x] **Cache the OpenAPI document per group-version.** `Paths()` and
  `Schema()` are both uncached in `client-go`, and `Discovery.topLevelFields`
  calls them per kind. A bare `kind` cluster has 65 listable kinds across 13
  group-versions, and the core/v1 document alone is 1.6 MB, so serving a whole
  cluster currently re-fetches and re-parses it once per core kind. This is the
  actual blocker for the goal above, not a nice-to-have.
- [x] **Collision-safe table naming.** `events` exists in both the core group
  and `events.k8s.io`. Under one schema per cluster both want the same table
  name, the second `CREATE` fails, and the whole `IMPORT` fails with it. Use the
  bare plural when it is unique and suffix the group only when ambiguous, so one
  collision does not uglify the other 64 names.
- [x] **`api_version`, `kind` and `metadata` as columns.** These are the only
  three fields guaranteed on every Kubernetes object — `spec` is present on 67%
  of built-in kinds and `status` on 47%, so neither is a safe basis for anything
  cross-kind. `apiVersion` and `kind` are currently dropped as "fixed by the
  table options" and `metadata` as "already exploded into scalars", but a query
  spanning kinds has nothing else to key on, and `metadata` carries fields not
  promoted individually (`ownerReferences`, `finalizers`, `deletionTimestamp`).

Notes from implementation: `--serve` now defaults to `*.*` ("narrow nothing"),
because RBAC became the boundary and two lists that must be hand-synced is the
arrangement this phase removed. It survives as optional narrowing.

**Discovery follows RBAC honestly, which surfaces more than the ClusterRole you
wrote**: Kubernetes binds `system:cluster-trust-bundle-discovery` to the
`system:serviceaccounts` group, so every ServiceAccount can list
`clustertrustbundles` and the gateway therefore offers it. The E2E asserts this
rather than working around it; it is also the clearest argument for keeping
`--serve` available. Access answers are cached for the gateway's lifetime (an
import asks about every kind at once, and RBAC does not change mid-import), so
a changed ClusterRole needs a restart — the gate asserts that path too.

Disambiguation is a property of the imported *set*, not a permanent rename: once
one half of the `events` collision is revoked, the survivor reclaims the bare
`events` name. The gate asserts that as well.

**A pre-existing read bug surfaced here**: `List` served any name filter with a
point `Get`, which for a namespaced kind with no namespace omits the namespace
segment entirely and 404s, so `WHERE name = 'x'` silently returned nothing
unless a namespace was also given. It now falls back to a `metadata.name` field
selector across all namespaces, keeping the point `Get` only where the object
can be named in full. `client-go`'s fake dynamic client ignores field selectors,
so the unit test asserts the selector is *sent* and the E2E covers the real
behaviour.

**E2E test (`e2e-phase5`)**: against a `kind` cluster with the Phase 4 test CRD
still applied, grant the gateway a deliberately partial RBAC set, then
`IMPORT FOREIGN SCHEMA` the whole cluster into one schema named for it. Assert:
every kind the ServiceAccount may list became a table and nothing else did
(including a kind that exists but is not granted); the `events` collision
produced two distinct, queryable tables; `api_version`/`kind`/`metadata` are
populated on a row from each of a built-in and a CRD; and the import issues one
OpenAPI fetch per group-version rather than one per kind (assert via a
gateway-side counter, the same technique the Phase 3 gate uses for List calls).
Then revoke one kind's RBAC and assert a re-import drops it.

---

## Phase 6 — The gateway as a Kubernetes workload, and a documentation site

**Goal**: run the gateway the way DESIGN.md §5.1 has always described it — a
Deployment inside the cluster it manages — and give Axiom user-facing
documentation that cannot silently drift from the code.

**Why now.** The gateway has never actually run in a cluster. It runs as a
compose container that joins kind's Docker network and authenticates with a
kubeconfig, so `rest.InClusterConfig()` has never executed, there is no
Deployment or Service manifest anywhere (`deploy/k8s/` holds only RBAC), and
projected ServiceAccount token mounting and rotation are untested. Phase 7's
impersonation work needs that real credential path underneath it, so this comes
first.

### Part 1 — Deploy the gateway into the cluster

**Postgres stays outside, deliberately.** Axiom exists so a database that cannot
reach the cluster's private network — including managed Postgres — can still
query it. Moving Postgres in-cluster for convenience would mask exactly the
routing, egress and TLS-boundary failures the design is meant to survive.

Tasks:
- [x] `Deployment` + `Service` manifests in `deploy/k8s/`, raw or Kustomize.
  **Not Helm, not cert-manager, not an operator** at this stage: cert-manager's
  webhook adds startup latency and flakes in kind, and Helm templating obscures
  the YAML diffs that make review possible.
- [x] Gateway runs on `rest.InClusterConfig()` with a projected ServiceAccount
  token, as non-root, with the RBAC already in `gateway-rbac.yaml`. Asserted:
  it starts on ambient pod credentials, with no `-kubeconfig` and no kubeconfig
  volume, as the `axiom-gateway` ServiceAccount, on a projected token carrying
  an expiry. **Not** asserted: surviving a rotation with watch streams intact.
  Kubelet refreshes at 80% of the token's lifetime and Kubernetes' floor is 600
  seconds, so observing one costs at least eight minutes of gate time; see the
  outcome note below.
- [x] TLS material delivered as a `Secret`, generated by the existing OpenSSL
  script rather than a new dependency. **The SANs must change**: they are
  `DNS:gateway,DNS:localhost,IP:127.0.0.1` today, none of which a client outside
  the cluster will connect to.
- [x] Reachability from outside: a `NodePort` fronted by kind's
  `extraPortMappings`, or the compose network routing to
  `<cluster>-control-plane:<nodePort>`. **Not background `kubectl port-forward`**
  — it drops long-lived HTTP/2 streams, which is precisely what the Phase 3
  watch gate depends on, and it orphans subshells when a script traps.
- [x] Build and `kind load` the image **once** in `e2e/run_all.sh`, not per gate.
  Per-gate loading would add 20-40s six times over.
- [x] Keep the inner dev loop. Requiring rebuild, load and rollout for every Go
  change turns a two-second edit into a minute. Keep a documented host-process
  mode (`go run ./cmd/gateway -kubeconfig …`) for development, with the
  in-cluster deployment as what CI and the gates exercise.
- [x] **Replace the log-scraping assertions.** `e2e/watch_test.sh` asserts an
  absolute count of `subscribe_list` lines over the gateway's container log,
  twice. A Pod restart starts a fresh log, so those assertions break the moment
  the gateway is a Deployment. They need a durable source — an exported counter
  or a debug RPC — which is better than log-grepping regardless.
- [x] Per-gate isolation: a gateway Pod surviving between gates carries its
  OpenAPI and access caches with it. Gates must reset it and wait for the
  rollout, the way `compose down -v` resets the stack today.

**E2E test (`e2e-phase6`)**: every existing gate passes with the gateway running
as an in-cluster Deployment rather than a compose container — that is the whole
point, so the regression suite *is* the test. Plus: assert the gateway is
running from a projected ServiceAccount token and not a kubeconfig, and that a
Pod restart mid-watch is survived (the Phase 3 property, re-asserted against a
real rollout).

### Part 2 — User-facing documentation that cannot drift

**What "auto-generated" can honestly mean.** Not architecture, not runbooks, not
threat models — those are written by people and reviewed by people. Generation
applies where code is genuinely the sole source of truth:
  - the gRPC API, from `.proto` comments;
  - server and foreign-table options, from the validators in
    `extension/src/options.rs`;
  - the column projection rules, from `extension/src/schema.rs` and its gateway
    counterpart;
  - the gateway's CLI flags, from the flag definitions.

Tasks:
- [x] `make docs-generate` emits the reference material above into
  `docs/generated/`.
- [x] A documentation site published to GitHub Pages, human-written guides
  alongside the generated reference.
- [x] **An anti-drift CI check, not a diff-police one.** CI runs
  `make docs-generate` and fails if `git diff --exit-code docs/generated/` is
  dirty. This repo already uses exactly that shape for `proto-check`, so it is
  consistent with existing practice. A rule like "every PR touching code must
  touch `docs/`" is the antipattern to avoid: it false-positives on every
  refactor and is satisfied by a whitespace change.
- [x] For the human half, a lightweight changeset: a note under `.changes/` for
  user-visible behaviour, with an explicit opt-out label for changes that are
  genuinely internal. Enforcement belongs in review, not in a regex over a diff.

### Found while using Phase 5 by hand

- [ ] **A whole-cluster `IMPORT` can exceed `rpc_timeout_secs`.** RBAC-bounded
  discovery made it materially slower: one access review per kind on top of the
  OpenAPI fetches. On a bare kind cluster with `--serve '*.*'` it exceeded a 10s
  timeout outright, and the 30s default is not obviously enough on a cluster
  with many kinds. Either give `IMPORT` its own longer budget, batch the access
  reviews, or both — but the current failure mode is a `Cancelled: Timeout
  expired` that gives the operator no hint that the fix is a timeout.
- [ ] **A removed CRD stays on offer until the gateway restarts.** Discovery
  invalidates and retries once on a *miss*, so a newly added CRD resolves with
  no restart — that direction is tested. A group-version's resource list, once
  fetched successfully, is never refreshed, so a kind that disappears keeps
  being offered and `IMPORT` keeps generating a table for it. Observed by
  uninstalling an operator and re-importing: its four kinds came back until the
  gateway was restarted. The fix is a bounded TTL on the cached resource list,
  or invalidating when a resolved kind starts returning NotFound.
- [ ] **Nothing tells an operator that a re-import is needed.** Changing RBAC or
  `--serve` and restarting the gateway changes what it offers, but foreign
  tables are catalog objects and do not move. The kinds simply fail to appear,
  with nothing pointing at the cause. Surfacing the drift — even just a
  `WARNING` when a scan hits a table the gateway no longer serves — would save
  the guesswork. This is also the friction the reconciler idea in
  `docs/AUTH.md`'s neighbourhood would remove entirely.

**E2E/CI test (`docs-check`)**: a deliberate change to a `.proto` comment and to
an FDW option fails CI until the generated docs are regenerated and committed;
a purely internal refactor does not.

### Phase 6 outcome

Done, except for the three rough edges above, which were found by using Phase
5 by hand and are carried into Phase 7 rather than fixed here: none of them is
about the gateway's deployment or its documentation, and folding them in would
have widened this phase past what it was for.

All six gates pass with the gateway as an in-cluster Deployment, and the suite
is the regression test for it. `make docs-check` is wired into CI beside
`proto-check`, and the site publishes from `main` on a change under `docs/`.

Two notes on what the credential assertion does and does not cover. The pods
gate proves the gateway runs with no `-kubeconfig` and no kubeconfig volume, as
the `axiom-gateway` ServiceAccount, on a projected token carrying an expiry —
that is what selects `rest.InClusterConfig()` and what makes rotation a real
concern rather than a static credential. It does **not** prove the gateway
survives an actual rotation: kubelet refreshes at 80% of the token's lifetime
and the floor Kubernetes permits is 600 seconds, so observing one costs at
least eight minutes of gate time. That is too long for the regression suite to
carry, and a gate that waits eight minutes to assert a negative is a gate
people will disable. It is left as a manual check against a long-running
deployment.

The generated pages are committed rather than built at publish time. The
anti-drift check needs them in the tree to have something to diff against, so
the Pages job publishes what is committed and never regenerates.

---

## Phase 7 — Per-caller identity and auth hardening

**Goal**: every SQL role's query reaches Kubernetes as a *distinct*, RBAC-scoped
identity, with the API server making the authorization decision. Today the
gateway holds one ServiceAccount and every Postgres user who can `SELECT` from a
foreign table gets all of it — the "single shared superuser token" DESIGN.md §7
exists to eliminate.

**Why after Phase 6 and before multi-cluster.** Phase 6 puts the gateway in the
cluster with an ambient ServiceAccount, which is the shape this phase's
impersonation work has to run in — developing it against a kubeconfig-in-a-
container would test the wrong credential path. And before multi-cluster
because: `CREATE USER MAPPING` is the per-cluster
credential mechanism (DESIGN.md §6), so building multi-cluster first would mean
every registered cluster shares one ambient trust relationship and then having
to revisit each one's credential story anyway. Multi-cluster also multiplies the
blast radius of a weak trust model from one cluster to N, and mTLS is far
cheaper to get right against one gateway than against several. Phase 5 made RBAC
the boundary for the *gateway's* identity; this phase makes it per *caller*,
which is the same thread (RULES.md §3).

### Design decisions to settle first

**The full analysis is in [AUTH.md](./AUTH.md)** — options considered and
rejected, the threat model, and an end-to-end flow. Summarised here so this
phase reads on its own; AUTH.md is authoritative where they differ.

These are load-bearing and cheap to decide now, expensive to discover halfway in.

**1. Authorization by impersonation.** For what a caller may *do*, the gateway
should impersonate rather than reimplement: hold a ServiceAccount permitted to
`impersonate` a bounded set of users/groups, and set the impersonation headers
per request. The API server then makes every authorization decision, the audit
log names the real principal, and the gateway needs no per-caller kubeconfig.
The gateway's own RBAC becomes `impersonate` over a narrow set rather than broad
access to resources — strictly less privilege than it holds today.

**1b. Authentication by gateway-minted token, with mTLS as the stronger
option.** RULES.md §3 forbids a credential travelling as a *payload field*. That
rules out putting a token in a request message, but not gRPC metadata, which is
transport-adjacent and is how Kubernetes itself carries bearer credentials. Both
a metadata token and an mTLS client certificate satisfy the rule.

The deciding factor is deployability. `ca_cert` is already a *file path* read by
the backend, so a private-CA gateway cannot be used from managed Postgres (RDS,
Cloud SQL, Azure Flexible Server) at all today, and a file-based client
certificate would lock that entire class out permanently. A token is a string: it
fits an option, a Secret, an environment variable. So:
  - the gateway mints tokens through a deliberately restricted flow — loopback
    or in-cluster only, the shape of `kubeadm token create` — and the operator
    puts one in `CREATE USER MAPPING`;
  - the token is **signed, not opaque**, carrying the principal as a claim. An
    opaque token would force the gateway to persist a token-to-identity table
    and replicate it across replicas; a signed one keeps it stateless, verifying
    with its own key. Revocation is then short expiry or a denylist;
  - mTLS stays supported for deployments that can manage PKI, since a bearer
    token has no proof of possession;
  - `ca_cert` gains an inline-PEM form. Managed Postgres needs it whichever
    authentication mechanism is chosen.

Two costs to accept openly rather than discover. A bearer token is replayable by
anything that can read it, and it now maps to Kubernetes privileges. And it is
stored in `pg_user_mapping` in plaintext, so it appears in `pg_dump` output — a
database backup would carry cluster credentials, where mTLS keeps only a path in
the catalog. Supabase Wrappers addresses this by storing a secret *reference*
rather than the secret; that indirection is worth evaluating before settling.

**2. The watch cache has no caller dimension, and that is a leak.** Phase 3
keys a subscription on `(endpoint, CA, kind, namespace)` and serves it to any
backend. Per-caller RBAC plus a shared cache means role B reads rows that role
A's identity fetched. Options, to be chosen deliberately:
  - key subscriptions on identity too, accepting one watch stream and one cache
    per identity (correct, potentially expensive);
  - restrict `cache_mode 'watch'` to servers whose user mapping resolves to a
    single shared identity, leaving per-caller tables on-demand (cheap, honest,
    narrower);
  - serve the cache only to the identity that requested it, treating others as
    a miss (simple, wasteful under fan-out).
  The narrow option is probably right for this phase, with the general one
  deferred — but it must be an explicit decision, and the chosen tier must be
  visible in `axiom_watch_status()` rather than silently applied.

**3. Kubernetes denies, it does not filter.** A cluster-wide `LIST` by an
identity without cluster-wide list permission returns 403 — it does not return
the subset that identity may see. So `SELECT * FROM prod.pods` for a
namespace-scoped role fails outright rather than returning that role's
namespaces. Either that becomes documented behaviour ("add a namespace qual"),
or the gateway discovers the caller's permitted namespaces and fans out per
namespace. The second is friendlier and costs a `SelfSubjectRulesReview` per
namespace, cacheable. Decide explicitly; do not let it emerge as a confusing
error.

**4. `NOTIFY axiom_events` is a global channel.** Any role may `LISTEN` and
learn that an object of a given kind, namespace and name changed, regardless of
whether it may read that object. Harmless with one shared identity, an
information leak with per-caller RBAC. Either scope the payload to what the
listener may see (Postgres `NOTIFY` cannot address a subscriber, so this means
dropping detail), or gate notifications behind a role and document it.

**5. Cached gRPC channels must key on the caller.** `CHANNELS` in the extension
is keyed `(Target, rpc_timeout)`. Add a client certificate and that key no
longer identifies the peer: within one backend, `SET ROLE` could hand one role a
channel authenticated as another. The key has to include whatever the user
mapping resolves to.

### Tasks

- [ ] Gateway mints signed, principal-bearing tokens through a loopback or
  in-cluster-only flow, and verifies them on every data-plane call. Unauthenticated
  connections are rejected outright.
- [ ] `CREATE USER MAPPING` carries the per-role credential; the gateway maps it
  to a Kubernetes identity and impersonates it for every call made on that
  caller's behalf.
- [ ] mTLS client certificates as an alternative to tokens, for deployments that
  can manage PKI and want proof of possession.
- [ ] `ca_cert` accepts inline PEM as well as a path, so a private-CA gateway is
  usable from managed Postgres, which cannot mount files.
- [ ] Decide how to keep the credential out of `pg_dump`: short expiry, a secret
  reference rather than the secret itself, or an accepted-and-documented risk.
- [ ] Enforce the privilege model from AUTH.md §6.1: a role granted `USAGE ON
  FOREIGN SERVER` can rewrite its own user mapping, so an identity stored there
  is self-serve unless query roles are given table grants only. Warn loudly on a
  configuration that holds both.
- [ ] Impersonate groups as well as a username, and carry the Postgres role,
  database and backend PID in `Impersonate-Extra-*` so cluster audit logs can
  attribute a request to a SQL session.
- [ ] Settle `current_user` versus `session_user` for the mapping lookup, and
  test the `SECURITY DEFINER` and view paths either way.
- [ ] Key the extension's channel cache on the resolved caller identity, so a
  role can never reuse a channel authenticated as another.
- [ ] Settle and implement the cache/caller interaction from decision 2, with
  the chosen tier surfaced in `axiom_watch_status()`.
- [ ] Settle and implement the deny-vs-filter behaviour from decision 3.
- [ ] Close the `CREATE SERVER` option surface. A role with delegated `USAGE ON
  FOREIGN DATA WRAPPER` can currently point a server at any endpoint, making the
  Postgres backend open TLS connections to a host of its choosing (server-side
  request forgery), and `ca_cert` is a server-read file path whose error text
  distinguishes a missing file from an unparseable one. Neither is reachable
  while only superusers may define servers, and both go live the moment that is
  delegated — which least privilege calls for. `postgres_fdw`'s
  `password_required` is the precedent: constrain what a non-superuser may put
  in server options, and require the credential to come from the user mapping
  rather than from the server's ambient position.
- [ ] Decide and document the `NOTIFY` exposure from decision 4.
- [ ] Audit: every gateway log line for a data-plane call names the impersonated
  principal, so "which SQL role read this" is answerable.
- [ ] Credential rotation: the gateway reloads its server certificate without a
  restart, and a rotated client certificate is picked up by the extension. The
  channel-eviction fix from Phase 5 covers the client half; the server half is
  new.

**E2E test (`e2e-phase7`)**: two Postgres roles with two `CREATE USER MAPPING`s
pointing at two RBAC-scoped identities on one cluster. Assert role A reads and
writes exactly what its identity permits; role B is denied out-of-scope
resources with a SQL error rather than a crash or an empty result; neither can
see the other's rows through a shared cache; a plaintext or
non-client-authenticated connection to the gateway is rejected; and the gateway
log attributes each call to the right principal. Add a negative test that a
non-superuser cannot define a server pointing at an arbitrary endpoint.

---

## Phase 8 — Multi-cluster

**Goal**: prove the `CREATE SERVER`-per-cluster abstraction actually holds up with
≥2 independent clusters/gateways simultaneously.

Tasks:
- [ ] bgworker: manage N independent persistent streams (one per configured
  server), independent reconnect/backoff state per cluster.
- [ ] Cache key already includes `cluster_id` (from Phase 3) — this phase is mostly
  a concurrency/isolation test, plus config surface (`CREATE SERVER ... OPTIONS
  (endpoint ...)` per cluster wired end to end).
- [ ] With Phase 5's schema-per-cluster model and Phase 7's per-caller
  credentials both in place, a second cluster is a second `CREATE SERVER`, a
  second `CREATE USER MAPPING`, and a second `IMPORT ... INTO <name>`. Naming
  and credentials are already settled, so this phase is isolation and
  concurrency only.
- [ ] Verify a failure/degradation in one cluster's stream doesn't affect another
  cluster's `ACTIVE` state or block its scans (isolation, not just "it also works").

**E2E test (`e2e-phase8`)**: two `kind` clusters in CI, two gateways, two `CREATE
SERVER`s. Query both, mutate one cluster's data, assert only that cluster's cached
table updates. Kill one gateway, assert the other cluster's `ACTIVE`/live queries
are unaffected (isolation test), then bring it back and assert it resyncs.

---

## Cross-cutting, applies to every phase

- Every phase's E2E test must run unattended in CI against a real `kind` cluster —
  no phase is "done" on the strength of manual testing alone.
- Each new phase's CI job re-runs all previous phases' E2E tests as regressions
  before declaring the new phase's own test authoritative.
- Track the open risks from DESIGN.md §8 (CRD schema explosion, watch/informer
  scaling, shared-memory eviction policy) as they become concretely testable —
  Phase 4 is the natural point to revisit CRD schema explosion, Phase 6 for
  informer scaling, and cache eviction should get its own task once a phase
  exercises a high-object-count cluster (not yet scheduled — flag if that becomes a
  near-term need rather than scheduling it speculatively now).
