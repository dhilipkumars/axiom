# Axiom — A Postgres FDW for Kubernetes

## 1. Summary

Axiom lets Postgres query and control Kubernetes resources (built-in and CRDs) as
foreign tables, across one or more clusters, from a Postgres instance that may live
entirely outside those clusters' networks. It supports read, write, and
watch-driven live updates — not just point-in-time polling.

## 2. Goals / Non-goals

**Goals**
- SQL read access to built-in and CRD resources, across multiple clusters.
- SQL write access (INSERT/UPDATE/DELETE) mapped to Kubernetes create/patch/delete,
  with optimistic-concurrency conflicts surfaced as SQL errors.
- Near-real-time reflection of cluster state via watch, not just poll-on-query.
- Postgres need not run inside, or even reach, the cluster's private network directly —
  only the gateway's exposed endpoint.
- Works with dynamic/unstructured CRDs, not just a hardcoded set of built-ins.

**Non-goals (for v1)**
- Transactional/serializable consistency between Postgres and cluster state. The
  cache is read-committed-ish and eventually consistent, bounded by watch latency —
  same posture `postgres_fdw` takes toward a remote server's own commit timeline.
- Multi-statement atomic transactions spanning multiple k8s objects (k8s itself has
  no cross-object transactions; we don't fake one).
- Full general-purpose Kubernetes admin UI/CLI replacement. This is a SQL interface,
  not a kubectl replacement.

## 3. Prior art (what we're building on)

- **Steampipe** (`steampipe-postgres-fdw` + `steampipe-plugin-kubernetes`): generic
  FDW core, gRPC to a per-source plugin process. Validates "keep the real client out
  of the Postgres process, talk to a sidecar/plugin over RPC." Read-only, poll-based —
  we extend this with watch-driven push and a write path.
- **Supabase Wrappers**: pgrx-based FDW SDK, including a Wasm-module variant for
  hot-swappable per-source logic without recompiling the extension. Considered as an
  alternative to a network gateway; rejected for v1 because Wasm modules still need
  network egress to the k8s API and don't solve auth/credential isolation as cleanly
  as a gateway that lives inside the cluster.
- **Multicorn**: same "generic core, per-source logic in a friendlier layer" lesson,
  older/Python.
- **`postgres_fdw` write semantics**: forward DML, let the remote system enforce its
  own constraints/concurrency, surface its errors — this is our model for mapping
  SQL writes to k8s PATCH + `resourceVersion` conflicts.

## 4. Architecture

```
 Kubernetes cluster                          Postgres host (outside the cluster)
 ┌─────────────────────────┐   gRPC (mTLS)   ┌───────────────────────────────────┐
 │ Gateway (Go, in-cluster) │◄───────────────►│ axiom (pgrx extension)             │
 │  - client-go informers   │  bidi stream    │  bgworker: owns tokio/tonic client,│
 │    per subscribed GVK    │  (push watch    │    one persistent stream per       │
 │  - CRD discovery via     │   events)       │    cluster/gateway                 │
 │    discovery/openapi     │                 │  shared-mem cache (dshash), keyed  │
 │  - unary Get/List/       │◄───────────────┐│    (cluster_id, gvk, ns, name)     │
 │    Create/Update/Delete  │  per-statement  ││  LISTEN/NOTIFY on cache change     │
 │    against k8s apiserver │  unary RPC      ││         │                         │
 └─────────────────────────┘                 │└─────────┼───────────────────────  │
                                              │   backend │ (per client connection)  │
                                              │   IterateForeignScan reads cache,   │
                                              │   or falls through to unary RPC     │
                                              │   ExecForeignInsert/Update/Delete → │
                                              │   unary RPC direct to gateway       │
                                              └────────────────────────────────────┘
```

**Why this shape** (see full brainstorm log for the reasoning, summarized here):
- pgrx + a real async k8s client (kube-rs/tokio) in *every backend* hits fork-safety,
  OpenSSL-linking, and async-in-sync problems. Solution: no k8s client code runs in a
  backend at all. All cluster-facing logic lives in the Go gateway; the extension only
  ever does local shared-memory reads or short gRPC calls.
- Postgres may be off-cluster with only egress network access. The gateway is the
  side with a stable, reachable endpoint (Service/Ingress); **Postgres always
  initiates the connection outward**, including the persistent watch stream. No
  inbound connectivity to the Postgres host is ever required.
- A SQL `SELECT` is pull-based — there's no way to push a row into a query that
  isn't running. So "push" targets a **standing cache** (updated by a dedicated
  bgworker's long-lived stream), not the query executor directly. Only the bgworker
  holds the persistent stream; per-connection backends never do.
- Multi-cluster support falls out of the existing FDW abstraction for free:
  one `CREATE SERVER` per cluster/gateway, `CREATE USER MAPPING` for its credentials.

## 5. Component design

### 5.1 Gateway (Go)

- One deployment per cluster it manages, in-cluster, fronted by a ClusterIP +
  Ingress/LB for external reachability from the Postgres host.
- Uses `client-go` informers for subscribed GVKs (built-in and CRD), and the
  discovery/OpenAPI client to resolve CRD schemas on demand.
- Exposes:
  - `Subscribe(gvk, namespace_filter, resourceVersion?) -> stream<WatchEvent>`
    (server-streaming, called from the bgworker's persistent connection).
  - `Get(gvk, namespace, name) -> Object`
  - `List(gvk, namespace_filter, label_selector?, field_selector?) -> []Object`
  - `Create/Update/Delete(gvk, namespace, name, body) -> Object | ConflictError`
  - `DiscoverSchema(gvk) -> ColumnSchema` (for CRDs — see §5.4)
- Holds the real kubeconfig/SA token; Postgres never holds cluster credentials
  directly, only credentials to the gateway itself (see §7).

### 5.2 pgrx extension (Rust)

- **bgworker**: one process, owns a tokio runtime + tonic gRPC client. Maintains one
  persistent bidi/streaming connection per configured cluster server. On event
  receipt, upserts into the shared-memory cache and issues `NOTIFY`. Handles
  reconnect/backoff and resync-from-`resourceVersion` bookmark on stream drop
  (same relist-watch hygiene as any k8s informer).
- **Shared-memory cache**: Postgres `dshash` keyed by `(cluster_id, gvk, namespace,
  name)`, value = last-known object bytes + `resourceVersion` + `last_event_ts` +
  tombstone marker. Subscription state tracked per `(cluster_id, gvk,
  namespace_filter)`: `{resourceVersion_bookmark, watch_status, last_full_list_ts}`.
- **FDW callbacks** (per-backend, no persistent state):
  - `GetForeignRelSize` / `GetForeignPaths`: inspect pushed-down quals to choose
    `Get` (exact namespace+name), cache-served `List` (active watch subscription
    exists), or `on_demand List` (no subscription / cold CRD) — see design log §
    "Decision at scan time."
  - `IterateForeignScan`: read from `dshash`, or issue a direct unary RPC.
  - `ExecForeignInsert/Update/Delete`: always a direct unary RPC to the gateway,
    never cache-served — writes must hit the live API server.

### 5.3 Consistency tiers

1. `LIVE` — active watch, cache-authoritative, zero RPC per scan.
2. `STALE-BUT-SERVEABLE` — watch degraded/reconnecting; cache still served but
   surfaced via a staleness marker (system column or a status function) — never
   silently lied about.
3. `ON-DEMAND` — no watch subscription; direct passthrough per query. Default for
   CRDs queried ad hoc; configurable per foreign table via `OPTIONS (cache_mode
   'watch' | 'on_demand')`.

Delete events tombstone rather than immediately evict, swept after a short delay to
avoid torn reads during a concurrent scan.

### 5.4 Schema mapping

- **Built-ins** (Pod, Deployment, Service, ConfigMap, Node, …): hand-mapped typed
  columns for the fields that matter (metadata.name, namespace, labels, status
  fields, spec fields commonly filtered/joined on), plus a catch-all `raw jsonb`
  column for everything else.
- **CRDs**: schema discovered from the cluster's CRD OpenAPI schema via the
  gateway's `DiscoverSchema` RPC. v1 approach: mostly `jsonb` columns
  (`spec jsonb`, `status jsonb`) plus promoted top-level scalar fields
  (name/namespace/labels/annotations/resourceVersion), rather than trying to
  fully explode arbitrary CRD schemas into typed SQL columns.
- **Where the mapping lives** (settled in Phase 4): the gateway decides *which*
  columns a kind gets; the extension decides what each column *means*, keyed on
  the column name alone. Neither side ships a per-kind table to the other, so a
  scan needs no schema round-trip and the two cannot disagree about a value —
  only about whether a column exists, whose worst case is a column that reads
  NULL. `IMPORT FOREIGN SCHEMA` writes the resolved `(group, version, kind)`
  into each generated table's options, which is what keeps discovery off the
  scan path entirely.
- **What a deployment serves** (added in Phase 4): removing the hardcoded kind
  registry would otherwise have widened the gateway to everything in the
  cluster, so the served set is an explicit `--serve` allowlist, with the
  ServiceAccount's RBAC scoped to match (§7, RULES.md §3). A kind outside it is
  reported exactly as a kind the cluster does not have, so the allowlist is not
  enumerable by probing.
- `IMPORT FOREIGN SCHEMA` support to auto-generate `CREATE FOREIGN TABLE` DDL by
  querying the gateway's discovery endpoint, rather than requiring hand-written DDL
  per resource type.

### 5.5 Write path

- SQL `INSERT` → gateway `Create`.
- SQL `UPDATE` → gateway `Update`, sent with the `resourceVersion` last read by that
  backend; a 409 Conflict from the API server surfaces as a SQL error (no silent
  retry/merge — same posture as `postgres_fdw` forwarding remote constraint
  violations).
- SQL `DELETE` → gateway `Delete`.
- No attempt to fake k8s transactionality across multiple statements.

## 6. Multi-cluster model

- `CREATE SERVER cluster1 FOREIGN DATA WRAPPER axiom_fdw OPTIONS (endpoint
  'https://gw.cluster1.example:8443')`
- `CREATE USER MAPPING FOR CURRENT_USER SERVER cluster1 OPTIONS (...)` — gateway
  credentials (see §7), not cluster credentials.
- `CREATE FOREIGN TABLE` per GVK per server, or generated via `IMPORT FOREIGN
  SCHEMA`.

## 7. Auth (gateway ↔ Postgres) — designed in Phase 6

**Full design: [AUTH.md](./AUTH.md).** This section is the summary.

Firmed up when Phase 6 was scheduled ahead of multi-cluster, on the grounds that
`CREATE USER MAPPING` is the per-cluster credential mechanism, so building
multi-cluster first would give every registered cluster one shared ambient trust
relationship and then require revisiting each. The shape:
- Transport: TLS always; mTLS where the deployment can manage client PKI.
- Principal: a credential held in `CREATE USER MAPPING`, mapped by the gateway to a
  Kubernetes RBAC identity (e.g. a `ServiceAccount` token or an OIDC-issued token the
  gateway exchanges), so Postgres-side SQL roles get real, auditable, scoped k8s RBAC
  — not a single shared superuser token for all Postgres users.
- **Authorization by impersonation.** The gateway impersonates rather than
  reimplements, so the API server makes every decision, the audit log names the
  real principal, and the gateway's own RBAC narrows to `impersonate` over a
  bounded set instead of broad resource access.
- **Authentication by a gateway-minted, signed token**, carried in gRPC
  metadata. §3 of RULES.md forbids a credential in a payload *field*, which a
  metadata header is not — this is how Kubernetes itself carries bearer
  credentials. The mechanism is chosen for deployability: `ca_cert` is currently
  a file path, so a private-CA gateway is already unusable from managed Postgres
  (RDS, Cloud SQL), and a file-based client certificate would exclude that class
  permanently. Signing keeps the gateway stateless; an opaque token would force
  it to persist and replicate a token-to-identity table. mTLS client certificates
  remain supported for deployments that can manage PKI and want proof of
  possession, which a bearer token does not give.
- **Kubernetes denies rather than filters.** A cluster-wide LIST by an identity
  without cluster-wide permission is a 403, not a filtered result, so per-caller
  identity changes query semantics for namespace-scoped roles. See PLAN.md
  Phase 6 for the options.
- **Per-caller identity and the shared watch cache are in tension.** The Phase 3
  cache is keyed `(cluster, kind, namespace)` with no caller dimension, so a
  shared cache would serve one identity rows another fetched. PLAN.md Phase 6
  lists the options and requires the chosen tier to be visible in
  `axiom_watch_status()` rather than silently applied.
- Out of scope for the POC phases below; flagged explicitly so it isn't
  accidentally skipped before any real cluster access is granted.

## 8. Explicit risks / open questions

- CRD schema explosion (deeply nested/oneOf-heavy CRDs) may force more JSONB and
  less typed-column mapping than we'd like — acceptable, not blocking.
  **Revisited in Phase 4 and closed for now**: the mapping never explodes a CRD
  schema at all, promoting metadata scalars and leaving every structured field
  as top-level `jsonb`, so a pathological CRD costs nothing beyond a wider row.
- **`NOTIFY axiom_events` is a global channel** (found while scoping Phase 6,
  open): any Postgres role may LISTEN and learn that an object of a given kind,
  namespace and name changed, whether or not it may read that object. Harmless
  under one shared identity; an information leak once identity is per-caller.
- **Server option surface** (found while scoping Phase 6, open): a role granted
  `USAGE ON FOREIGN DATA WRAPPER` can point a `CREATE SERVER` at any endpoint,
  making the Postgres backend open TLS connections to a host it chose, and
  `ca_cert` is a server-read path whose errors distinguish missing from
  unparseable. Unreachable while only superusers may define servers; live the
  moment that is delegated, which least privilege calls for.
- **Status subresources** (found in Phase 4, open): a kind with
  `subresources: status` has its status ignored by a PUT to the main resource,
  so a SQL `UPDATE` of a `status` column would appear to succeed and change
  nothing. Writing status needs its own request against the status subresource.
  Not yet scheduled; the Phase 4 test CRD deliberately has no status
  subresource so the gate tests the path that is actually implemented.
- Watch scalability: many foreign tables × many namespaces could mean many informers
  in the gateway; needs shared-informer-factory style de-duplication, not one
  raw watch per foreign table.
- Cache memory bound in Postgres shared memory: needs an eviction/LRU policy for
  clusters with very large object counts — not designed in detail yet, flagged for
  Phase 3.
