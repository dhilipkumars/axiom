# Axiom — Engineering Rules

Binding for every phase in [PLAN.md](./PLAN.md). A phase is not "done" until it
satisfies all four; these are gates, not aspirations. CI should enforce as much of
this mechanically as possible rather than relying on review discipline alone.

## 1. Highest quality

- No `unwrap()`/`expect()`/`panic!` in the Rust extension outside of `_PG_init`
  startup-invariant checks. A panic inside a backend takes that connection down;
  inside the bgworker it takes the cache/watch state down. Every fallible path
  (RPC call, shared-memory access, deserialization) returns a `Result` and
  surfaces as a proper SQL error via pgrx's error-reporting path.
- No unchecked Go error returns (`_ = err` / discarded errors) — `errcheck` in CI,
  zero exceptions without an inline justification comment.
- `clippy::pedantic`-clean (with an explicit, reviewed allow-list, not a blanket
  suppression) for the Rust crate; `golangci-lint` (`errcheck`, `govet`, `staticcheck`,
  `gosec` — see rule 3) clean for the gateway.
- No silent fallback/degrade-and-continue behavior. If the cache is stale, say so
  (per DESIGN.md's consistency tiers) — never mask it as fresh data. If a phase's
  code takes a shortcut, mark it explicitly (`// TODO(phaseN): ...`) rather than
  leaving it indistinguishable from a considered decision.
- Every public RPC, FDW callback, and cache-state-transition function gets a doc
  comment stating its contract (inputs, error conditions, side effects) — not what
  the code obviously does line-by-line.
- No task in PLAN.md is considered complete while it has open `clippy`/`lint`
  warnings, `TODO`s without a tracking note, or disabled tests.

## 2. Unit-testable and integration-testable by construction

- **Design for testability up front, not bolted on**: gateway k8s-facing code goes
  behind a narrow interface (`type K8sClient interface { Get/List/Watch/Create/... }`)
  so unit tests use a fake implementation (or `client-go`'s built-in fake
  clientset) with zero real cluster required. Extension code separates "pure" logic
  (qual-to-RPC-filter translation, cache-tier decision logic, schema mapping) from
  "impure" glue (actual FDW callback / actual dshash access) so the pure logic is
  unit-testable in plain Rust `#[test]`s without a running Postgres.
- **Unit tests, per component**:
  - Gateway: every RPC handler has a table-driven test against the fake clientset,
    including error paths (not-found, conflict/409, malformed input).
  - Extension: qual pushdown logic, cache-tier selection (LIVE/STALE/ON-DEMAND),
    tombstone/sweep logic, and schema-mapping (JSON → column) all get pure Rust
    unit tests independent of a live Postgres or gateway.
- **Integration tests** (real Postgres + real (or `kind`) cluster, no mocks) sit
  between unit tests and the phase's full E2E test — e.g. "gateway process +
  real `kind` cluster, RPC-level assertions" without Postgres in the loop yet, and
  "extension + real Postgres + a stub gateway" without a real cluster. These
  exist so a failure localizes to one side of the gRPC boundary instead of only
  ever being diagnosable from a full E2E failure.
- **Coverage bar**: new code in a phase ships with unit tests covering its
  error-handling branches, not just the happy path — a PR that only tests the
  success case does not satisfy this rule. No specific numeric coverage threshold
  is prescribed here; "every branch that can fail has a test that makes it fail" is
  the actual bar.
- CI runs unit tests, integration tests, and the phase's E2E test as three
  distinct, separately-reportable jobs — a green E2E run never substitutes for
  missing unit coverage.

## 3. No security loopholes — highest security standard

- **Least privilege, always**: the gateway's own k8s RBAC (its `ServiceAccount`)
  is scoped to exactly the GVKs/verbs/namespaces a given deployment is configured
  to serve — never a cluster-admin binding for convenience, even in the POC phases.
  Extend this per-caller once Phase 6 auth lands (per-Postgres-role RBAC identity,
  DESIGN.md §7) — Phase 6 is not "add auth," it's "add the *per-caller* layer" on
  top of a gateway that was already least-privilege from Phase 0.
- **Every trust boundary gets an explicit control, from Phase 0**:
  - Postgres ↔ gateway: TLS from the first phase that has a real network hop
    (not deferred to "later hardening" as plaintext-then-retrofit) — Phase 6
    upgrades this from TLS to mTLS + RBAC-mapped identity, it doesn't introduce
    encryption that wasn't there before.
  - Gateway ↔ k8s API server: standard `client-go` in-cluster or kubeconfig auth,
    token never logged, never included in error messages returned to Postgres.
  - No credential (kubeconfig, SA token, mTLS key) ever traverses the gRPC
    protocol as a payload field — credentials are held locally by each side and
    referenced, never transmitted through the data-plane RPCs.
- **Input handling**: all data crossing the gRPC boundary in either direction is
  untrusted input to the receiver — the gateway must not deserialize
  attacker/caller-controlled GVK or field-selector strings into anything that can
  reach `exec`/shell/template evaluation; the extension must not trust object
  JSON from the gateway enough to skip bounds/size checks before shared-memory
  writes (a large/malicious object must not be able to overrun or exhaust the
  `dshash` segment).
- **No SQL injection surface**: SQL-side identifiers (table/column names generated
  by `IMPORT FOREIGN SCHEMA` from CRD names) are strictly sanitized/quoted per
  Postgres identifier rules before being used in generated DDL — CRD names are
  attacker-influenceable in a multi-tenant cluster, treat them accordingly.
- **Static + dependency scanning in CI**: `cargo audit` / `cargo deny` for the Rust
  crate, `govulncheck` + `gosec` for the gateway, on every PR — not just at release
  time. A finding blocks merge unless explicitly triaged and documented as accepted
  risk (never silently ignored).
- **Secrets hygiene**: no credential ever appears in a log line, panic message, SQL
  error text, or test fixture committed to the repo. CI includes a secret-scan
  (e.g. `gitleaks`) on every push.
- **Phase 6 is a hardening phase, not the *only* security phase**: rules above
  apply from Phase 0 onward. Phase 6's job is specifically the per-caller RBAC
  mapping and mTLS upgrade described in DESIGN.md §7 — everything else in this
  section is a standing requirement for every phase, including the POC ones.

## 4. Every phase ships fully E2E-tested and hardened before moving on

- A phase is not "done" when its feature works once locally — it's done when:
  1. Its E2E test (per PLAN.md) passes unattended in CI against a real `kind`
     cluster.
  2. All prior phases' E2E tests still pass (regression gate) in the same CI run.
  3. Rules 1–3 above are satisfied for all code introduced in that phase — no
     "we'll harden it in a later phase" deferral for anything that isn't
     explicitly scoped as a later phase's job in PLAN.md (e.g. mTLS is
     legitimately Phase 6's job; a `panic!` on a malformed RPC response in Phase 1
     is not something any later phase is scoped to clean up, so it must be fixed
     in Phase 1).
- No phase is reopened to bolt on tests/hardening after the fact as a separate
  cleanup task — the testing and hardening for a phase's own code is that phase's
  work, not a follow-up.
- If a phase's E2E test starts flaking, that is treated as a bug in that phase's
  code or test harness, not muted/skipped to unblock the next phase.
