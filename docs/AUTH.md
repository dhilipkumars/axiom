# Axiom — Authentication and Authorization Design

Companion to [DESIGN.md](./DESIGN.md) §7, which sketched this as a placeholder.
This document settles it, because the choice constrains both remaining phases:
Phase 6 implements it, and Phase 7 (multi-cluster) inherits whatever shape it
takes.

The question is narrow but consequential: **when a SQL user queries a foreign
table, whose Kubernetes identity performs the read?**

---

## 1. What exists today, and why it is not enough

The gateway holds one ServiceAccount. Every Postgres user who can `SELECT` from
a foreign table gets all of it. There is no caller identity on the wire at all —
the extension sends a GVK and filters, and the gateway acts as itself.

Phase 5 narrowed what that single identity may see, by making discovery follow
the ServiceAccount's own RBAC. That is the *gateway's* least privilege. It does
nothing for the *caller's*: two Postgres roles querying the same table get
identical results and identical write powers, whoever they are.

So the gap is per-caller scoping, and RULES.md §3 names it precisely — Phase 6
is "add the per-caller layer on top of a gateway that was already
least-privilege from Phase 0."

## 2. Trust boundaries

| Boundary | Today | Phase 6 |
|---|---|---|
| SQL user → Postgres | Postgres roles and `pg_hba` | unchanged |
| Postgres → gateway | TLS, server-authenticated only | caller identity established |
| Gateway → API server | one ServiceAccount token | per-caller identity |
| SQL user → `CREATE SERVER` | superuser-only by default | option surface constrained |

The fourth row is easy to overlook. Defining a foreign server is a privileged
act: it chooses which gateway — and therefore which cluster's credentials — this
database will speak to. Postgres gates it behind `USAGE` on the foreign-data
wrapper, which defaults to superuser-only and is delegable by `GRANT`.

## 3. Constraints that rule options out

**C0. `current_user` is not the session's login role.** Inside a
`SECURITY DEFINER` function, or when reading a view, `current_user` is the
owner. Any mapping lookup keyed on it inherits that switch.

**C1. No credential in a payload field.** RULES.md §3. This forbids putting a
token in a request message. It does *not* forbid gRPC metadata, which is
transport-adjacent and is how Kubernetes itself carries bearer credentials.

**C2. Managed Postgres cannot mount files.** RDS, Cloud SQL and Azure Flexible
Server give no filesystem access. Any credential that is a *path* excludes that
entire class. This already bites: `ca_cert` is a path today, so a private-CA
gateway cannot be used from managed Postgres at all.

**C3. Foreign server and user mapping options are dumped.** Verified, not
assumed — `pg_dump` emits them verbatim:

```
CREATE USER MAPPING FOR axiom SERVER secrettest OPTIONS (
    password 'SUPER-SECRET-TOKEN',
    "user" 'app'
);
```

A secret placed in either lands in every backup. The live catalog is better
protected: a non-superuser sees `umoptions` as NULL for mappings it does not
own. The exposure is backups, not querying.

**C4. Postgres has no per-statement credential hook.** A user mapping is static
DDL. Nothing refreshes it on a timer, so a short-lived secret stored there
cannot renew itself without additional machinery.

**C5. A role with `USAGE` on a foreign server can rewrite its own user
mapping.** Verified, not assumed. PostgreSQL lets a user create or alter a
mapping *for their own name* once `USAGE ON FOREIGN SERVER` is granted. With
`USAGE`, an `ALTER USER MAPPING` reached the FDW's option validator rather than
a permission error — it was refused only because no user-mapping options exist
yet. After `REVOKE USAGE`, the same statement failed with `42501`, and the role
could still `SELECT` from the foreign table on a plain `GRANT SELECT`.

This is load-bearing for §6: it means any identity stored in a user mapping is
self-serve unless the query role is denied `USAGE` on the server. See §6.1.

**C6. The watch cache has no caller dimension.** Phase 3 keys a subscription on
`(endpoint, CA, kind, namespace)` and serves it to any backend. Per-caller
identity plus a shared cache is an information leak, not merely an
inefficiency.

---

## 4. Two orthogonal questions

Authentication — *who is calling?* — and authorization — *what may they do?* —
are independent, and conflating them is how these designs go wrong. Take them
separately.

## 5. Authorization: how the gateway enforces what a caller may do

**A1. Gateway reimplements RBAC.** Read the caller's permissions and filter
results itself.
*Rejected.* Reimplementing Kubernetes authorization is a large, subtle surface
that must track upstream forever, and any divergence is a security bug.

**A2. Per-caller kubeconfig.** Hold a distinct client credential per caller.
*Rejected.* Requires managing N credentials against the cluster, with rotation
for each. Does not scale and duplicates what the API server already models.

**A3. Impersonation.** The gateway's ServiceAccount is granted `impersonate`
over a bounded set, and sets impersonation headers per request.
**Recommended.** The API server makes every authorization decision — the only
authority that can make it correctly. The audit log records both the real and
impersonated principal. The gateway's own RBAC *narrows* to `impersonate` over
an enumerated list, which is strictly less than the broad resource access it
holds today:

```yaml
- apiGroups: [""]
  resources: ["users"]
  verbs: ["impersonate"]
  resourceNames: ["alice@corp.example", "bob@corp.example"]
- apiGroups: [""]
  resources: ["groups"]
  verbs: ["impersonate"]
  resourceNames: ["platform-readers"]        # see below
```

**Groups are not optional.** Impersonating only `UserName` gives the request
`system:authenticated` and nothing else, so every RBAC binding that grants
through a group — corporate IdP groups, and most real-world cluster policy —
stops applying and calls fail with 403 that look like a bug in Axiom. Deciding
*which* groups a principal may be impersonated with is part of this design, not
an implementation detail, because impersonating a group is itself an escalation
path: whoever may impersonate `system:masters` is cluster-admin.

**`Impersonate-Extra-*` carries the forensics.** The API server audit log
records the gateway's ServiceAccount and the impersonated username, and nothing
that ties a request back to the database. Extras should carry the Postgres role,
database, backend PID and client address, so "which SQL session read this
Secret" is answerable from the cluster audit log alone.

**A consequence worth designing for: Kubernetes denies, it does not filter.** A
cluster-wide `LIST` by an identity without cluster-wide permission returns 403,
not the subset that identity may see. So `SELECT * FROM prod.pods` fails
outright for a namespace-scoped role rather than returning their namespaces.
Two answers: document it ("add a namespace qual"), or have the gateway discover
the caller's permitted namespaces and fan out per namespace. The second is
friendlier and costs a `SelfSubjectRulesReview` per namespace, cacheable.

## 6. Authentication: how the gateway learns who is calling

**B1. mTLS client certificate per role.** The certificate's subject is the
principal.
*Strong but excluded as the default by C2* — a client certificate and key are
files, so managed Postgres cannot use it. Proof of possession is its real
advantage: nothing replayable is stored anywhere. **Keep as a supported option**
for deployments that can manage PKI.

**B2. Per-role bearer token, minted by the gateway.** Each role's user mapping
holds its own token.
*Workable but weaker than it looks.* It satisfies C1 via metadata and C2 by
being a string, but collides with C3 (every token in every backup) and C4
(rotation is DDL per role). N roles means N secrets to rotate.

**B3. Forward the caller's own Kubernetes token.** The SQL user supplies their
own token.
*Rejected.* Pushes credential management onto every end user, and the token
still has to be stored somewhere Postgres can read.

**B4. Instance credential plus asserted principal.** One signed credential
authenticates the *Postgres instance*; the extension asserts *which role* is
calling, in request metadata; the gateway impersonates that principal.
**Recommended.** One secret per instance rather than per role. The user mapping
holds only an identity — `OPTIONS (k8s_user 'alice@corp.example')` — so nothing
sensitive is in the catalog and C3 disappears. Rotation is one `ALTER SYSTEM`
with no per-role DDL, dissolving C4.

The obvious objection is that an instance could assert any principal. That is
why **the credential must carry its own bound**: the signed token includes a
`may_assert` claim and an `aud` naming the gateway and cluster it is for, so a
stolen token cannot claim `system:masters` and cannot be replayed against a
different gateway that shares a signing key.

### 6.1 The privilege model is part of the design, not an afterthought

Per C5, a role granted `USAGE ON FOREIGN SERVER` can rewrite its own mapping —
including `k8s_user`. Left unaddressed that is a silent escalation to any
principal matching `may_assert`, which would defeat the whole scheme.

The mitigation is verified and cheap, but it must be *stated*, because the
failure is invisible:

- the DBA owns the foreign server and creates every user mapping;
- query roles are granted **`SELECT`/`INSERT`/`UPDATE`/`DELETE` on the foreign
  tables only**, never `USAGE ON FOREIGN SERVER`;
- with that split, a role can query normally and cannot touch its own mapping.

Phase 6 should also refuse to start, or warn loudly, when a role holds both
`USAGE` on a server and a mapping carrying an identity — the configuration is
indistinguishable from an escalation waiting to happen.

**`current_user` versus `session_user`** (C0) is the second half of the same
question. Keying the lookup on `current_user` means a `SECURITY DEFINER`
function or a view owned by a privileged role lends its Kubernetes identity to
whoever calls it. Keying on `session_user` blocks that but also breaks
legitimate abstraction: a view that exposes a curated slice of a cluster is a
reasonable thing to build. `postgres_fdw` uses the effective user. Decide
explicitly, document it, and test it either way.

The token must be **signed rather than opaque**. An opaque token forces the
gateway to persist a token-to-identity table and replicate it across replicas;
a signed one is verified with the gateway's own key and carries its claims.

**What this trusts.** That Postgres reports the calling role honestly. This is a
smaller shift than it appears: under B1 and B2 alike, anyone who can read the
credential can act as that role. B4 makes the trust explicit and bounds it. The
extension reads `current_user` in-process and looks the mapping up by it, so a
role cannot assert another's identity. A Postgres superuser can `SET ROLE` to
anyone and therefore reach any mapped identity — true under every option here.

## 7. Where the instance credential lives

C3 rules out server and user mapping options. The remaining candidates:

**S1. A superuser-only GUC.** `ALTER SYSTEM SET axiom.instance_token = '…'`
writes `postgresql.auto.conf`, which `pg_dump` does not touch, and
`GucFlags::SUPERUSER_ONLY` keeps it out of `SHOW` for ordinary roles. The
mechanism already exists in this codebase for `axiom.notify_database`.
**Recommended for self-managed Postgres**, with two caveats that C2 forces:

- **`ALTER SYSTEM` is unavailable on managed Postgres.** RDS and Cloud SQL do
  not grant true superuser and block it; the value has to arrive through a
  parameter group instead. That works for extension-defined parameters but is a
  different operational path, and the design must say so rather than imply
  `ALTER SYSTEM` everywhere.
- **`postgresql.auto.conf` is not replicated.** A physical streaming replica
  does not inherit it, so a read replica running the extension has no
  credential until one is set locally. Worth stating before someone discovers it
  during a failover.

**S2. A file.** Simple, but excluded as a default by C2.

**S3. An external secret manager.** The GUC or option holds a reference such as
`vault://axiom/prod` and the extension fetches at connect time. Strongest for
organisations that already centralise secrets, and it composes with S1. Costs a
network dependency on the connect path and a bootstrap credential for the
manager itself, usually an instance IAM role. **Worth supporting later; not
required first.**

**S4. Accept the exposure and document it.** *Rejected.* Dumps travel to
laptops, object storage and CI. Turning a backup into cluster access is a
different risk from data disclosure.

---

## 8. Recommended design, end to end

Setup, once per Postgres instance:

```
axiom-gateway issue-instance-token --name pg-prod --may-assert '*@corp.example'
```

```sql
ALTER SYSTEM SET axiom.instance_token = '<token>';   -- not dumped
SELECT pg_reload_conf();

CREATE SERVER prod FOREIGN DATA WRAPPER axiom_fdw OPTIONS (endpoint 'https://gw.prod:8443');
CREATE USER MAPPING FOR alice SERVER prod OPTIONS (k8s_user 'alice@corp.example');
```

Per query:

```
alice: SELECT * FROM prod.pods WHERE namespace = 'payments';
   │
   │ 1. FDW callback in alice's backend looks up the user mapping for
   │    current_user. alice cannot supply k8s_user herself.
   │ 2. gRPC List over TLS, two metadata headers:
   │       authorization:     Bearer <instance token>
   │       x-axiom-principal: alice@corp.example
   ▼
gateway
   │ 3. Verify signature and expiry            -> instance pg-prod
   │ 4. Check principal against may_assert     -> permitted
   │ 5. Impersonate{UserName: alice@corp.example}
   ▼
API server
   │ 6. Authorizes as alice. Audit records gateway-sa impersonating alice.
   ▼
   7. 403 -> PERMISSION_DENIED -> SQLSTATE 42501.
```

**Fail closed.** A role with no user mapping must be denied. Today it would fall
back to the gateway's own ServiceAccount, which under this model is a silent
privilege escalation.

**Channel cache must key on the principal.** The extension caches channels on
`(Target, rpc_timeout)`. Add a caller identity and that no longer identifies the
peer: within one backend, `SET ROLE` could hand one role a connection
authenticated as another.

**`ca_cert` needs an inline-PEM form** regardless of which option wins, because
C2 applies to it too.

## 9. Open questions

**Q1. One GUC, many gateways.** Multi-cluster means one instance token per
gateway, but a GUC holds one value and pgrx defines GUCs at `_PG_init`, so
per-server GUC names are not available. A single superuser-only GUC holding a
map — `axiom.instance_tokens = 'prod=…,staging=…'` — is workable and dump-safe
but inelegant. Settle before Phase 7.

**Q2. Cache and caller.** Per C6. The tension is sharper than "pick a tier":
Kubernetes authorization is per-object and dynamic, so a genuinely shared cache
is only safe if every read is checked, which removes the reason the cache
exists. That argues for restricting `cache_mode 'watch'` to servers whose
mapping resolves to a single shared identity, and leaving per-caller tables
on-demand. Whichever is chosen must be visible in `axiom_watch_status()` rather
than applied silently.

**Q3. Deny versus filter.** Per §5. Fanning out is more expensive than it
first appears: the impersonated caller usually cannot list namespaces at all, so
the gateway must enumerate them as itself and then run one
`SelfSubjectRulesReview` per namespace as the impersonated user. On a cluster
with hundreds of namespaces that is a burst of reviews per query unless cached,
and a fan-out of unindexed LISTs after it. Documenting "add a namespace qual" is
the cheap answer and may simply be the right one.

**Q4. `NOTIFY axiom_events` is global.** Any role may `LISTEN` and learn that a
named object of a given kind changed, whether or not it may read it. Harmless
under one shared identity; a leak under per-caller identity.

**Q5. Server option surface.** A role granted `USAGE ON FOREIGN DATA WRAPPER`
can point a server at an arbitrary endpoint, making the backend open TLS
connections to a host it chose, and `ca_cert` is a server-read path whose errors
distinguish missing from unparseable. `postgres_fdw`'s `password_required` is
the precedent for constraining this.

**Q6. Revocation.** Signed tokens are not revocable without expiry or a
denylist, and an instance token in a GUC cannot rotate frequently without a
reload — so it is effectively a master key for its whole validity window. Either
the window is short and something automates the reload, or the gateway keeps a
denylist and gives up some of the statelessness §6 bought. Pick one; the current
text wants both.

**Q7. Connection poolers.** A transaction-mode pooler such as PgBouncer
multiplexes distinct client sessions through one backend, and `DISCARD ALL`
resets Postgres session state but not the extension's process-level statics.
Keying the channel cache on the resolved principal makes reuse correct, but any
future per-session state in the extension needs the same treatment, and this
should be tested rather than reasoned about.

## 10. What this means for multi-cluster

Phase 7 adds a second `CREATE SERVER`, a second `CREATE USER MAPPING` per role,
and a second instance token. Only Q1 stands between this design and that being
mechanical — which is the reason Phase 6 was moved ahead of Phase 7.
