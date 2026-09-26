# Giving an AI agent access

An agent that can see a cluster can answer questions no dashboard anticipated.
With `kubectl`, that means handing it a shell and a kubeconfig. With Axiom, it
gets a Postgres role and nothing else. This page shows how to set that role up
so it reaches exactly what you intend. It also covers the controls that look
like security but aren't, and what Axiom does not yet enforce.

Everything under [The recipe](#the-recipe) is run against a real cluster by
the `agent` end-to-end test (`e2e/agent_test.sh`), so CI checks every claim
it makes.

## What an agent gets that kubectl cannot give it

- **No shell.** A `kubectl`-based agent needs a shell tool, and a shell exposes
  `kubectl exec`, `kubectl cp` and debug pods to anything that can steer the
  agent's prompt. An Axiom agent speaks SQL, and only SQL.
- **No Kubernetes credential in the agent's environment.** The gateway holds
  its ServiceAccount inside the cluster. The agent holds a Postgres login, so
  there's no Kubernetes token for it to leak.
- **Field-level redaction.** Kubernetes RBAC stops at the resource: `get pods`
  returns the whole object, including env vars that often carry credentials
  inline. A view or a column grant can expose `phase` and `node` and leave the
  rest out entirely.
- **Scoping that RBAC cannot express.** A view can limit an agent to "the
  namespaces belonging to customer X, according to our own tenants table".
- **No Secrets.** The shipped gateway RBAC never grants them, so no table
  exists to leak them.
- **Writes that fail rather than overwrite.** An `UPDATE` carries the
  `resourceVersion` it read. If the object has changed since, the statement
  fails with `40001` rather than overwriting the change.
- **Attribution by role.** `log_statement` or `pgaudit` records every
  statement against the role that ran it.

## The recipe

Give the agent a schema of views, owned by a trusted role, and grant it
nothing else. Each view chooses the columns the agent sees and the rows it can
reach:

```sql
CREATE ROLE agent_role LOGIN PASSWORD '...';

CREATE SCHEMA agent;
CREATE VIEW agent.configmaps WITH (security_barrier) AS
  SELECT namespace, name, labels, creation_timestamp       -- no data, no raw
    FROM k8s.core_configmaps
   WHERE namespace IN (SELECT namespace FROM tenants WHERE customer = 'Acme');

GRANT USAGE ON SCHEMA agent TO agent_role;
GRANT SELECT ON agent.configmaps TO agent_role;
```

What the test shows the agent can and cannot do:

| The agent tries to | Result |
|---|---|
| read its tenant's objects through the view | works, and only that tenant's |
| read a column the view leaves out (`data`) | `column does not exist` |
| read the table behind the view, or its `raw` | permission denied |
| `UPDATE` or `DELETE` through the view | permission denied |
| turn off read-only mode and write anyway | permission denied |
| create a foreign table of its own | permission denied |
| call `axiom_watch_status()` | permission denied |

`security_barrier` keeps the planner from evaluating a function the agent
supplies before the view's own `WHERE`, where it could see rows the view
filters out.

**Scoping has a cost.** Only a literal `namespace = '…'` is pushed down to the
gateway. A view that filters through a subquery, like the one above, makes the
gateway list every namespace, and Postgres then filters. The result is
correct, but costs a cluster-wide list. When the scope is fixed, write the
namespaces out literally, or `UNION ALL` one branch per namespace.

### Letting an agent write one field

To let an agent change one thing, grant that column on the table and nothing
more:

```sql
GRANT USAGE ON SCHEMA k8s TO writer_role;
GRANT SELECT (namespace, name), UPDATE (data) ON k8s.core_configmaps TO writer_role;
```

The role can update `data` without being able to read `data` or `raw`. Axiom
carries the object's identity and `resourceVersion` in a hidden copy of `raw`,
which Postgres adds after it has checked privileges. The test confirms the
write reaches the cluster and that the role still cannot read either column.

The gateway's RBAC must also allow the write. The shipped RBAC allows it for
ConfigMaps and nothing else.

## Controls that are not controls

- **`default_transaction_read_only` and `statement_timeout`.** Both can be set
  per session, so an agent can undo them with `BEGIN READ WRITE` or
  `SET statement_timeout = 0`. The test does exactly that. What stops the
  write is the absence of a grant, so make an agent read-only by not granting
  `INSERT`, `UPDATE` or `DELETE`.
- **The gateway's RBAC, as a per-agent limit.** It's a ceiling shared by every
  role. Until [#71](https://github.com/dhilipkumars/axiom/issues/71), every
  Postgres role reaches Kubernetes as the same ServiceAccount, so the Postgres
  grant is the only control that tells one agent from another.
- **`BEGIN … ROLLBACK` as a dry run.** Kubernetes writes are not part of the
  Postgres transaction. `ROLLBACK` does not undo them, and neither does
  anything else. `EXPLAIN ANALYZE` on an `UPDATE` also executes it. If a human
  should approve a write, the approval has to happen before the statement
  runs.

## Never grant an agent

- **`USAGE ON FOREIGN SERVER`.** It lets a role rewrite its own user mapping.
  It also lets the role create a foreign table with a `raw` column, which
  reads everything the gateway can and bypasses every view and column grant.
- **`USAGE ON FOREIGN DATA WRAPPER`.** With it, `CREATE SERVER` makes the
  Postgres backend connect to any address it's given, and a `ca_cert` path
  can probe the database host's filesystem.
- **`UPDATE (raw)`.** `raw` is the whole object, so this undoes any field-level
  redaction.
- **Ownership of the views or tables, or superuser.** Either bypasses all of
  the above.
- **`EXECUTE` on `axiom_watch_status()`.** It isn't granted by default, because
  it names every watched resource and namespace whatever the role's other
  grants. Keep it for monitoring roles.

## What Axiom does not yet enforce

- **Kubernetes can't tell agents apart.** Every read and write reaches the API
  server as the gateway's ServiceAccount, so the Kubernetes audit log
  attributes all of it to `axiom-gateway`. Reads served from the watch cache
  don't reach the API server at all. The Postgres log is the audit trail.
  [#71](https://github.com/dhilipkumars/axiom/issues/71) adds per-role
  identity.
- **Row-level security doesn't apply to foreign tables.** Views are the
  mechanism for scoping rows, as above.
- **`LISTEN axiom_events` is global.** Any role can listen, and learn which
  objects changed even in tables it cannot read. Notifications can also be
  lost, so don't build an agent's control loop on them.
- **No limit on how many rows a write touches.** A `DELETE` with a broad
  `WHERE` removes everything it matches, one API call per row. Grant `DELETE`
  only where that is acceptable.

## Traps an agent can walk into

These aren't security issues, but they're worth putting in the agent's
instructions:

- **Numbers are text.** `replicas > '5'` compares strings, so `'10' < '5'`.
  Cast (`replicas::int > 5`), and use `axiom_quantity()` for Kubernetes
  quantities such as `500m` or `128Mi`
  ([#79](https://github.com/dhilipkumars/axiom/issues/79)).
- **`INSERT` ignores `raw`.** Supply the typed columns instead
  ([#78](https://github.com/dhilipkumars/axiom/issues/78)).
- **Table names carry the API group.** It's `k8s.core_pods`, not `k8s.pods`,
  unless someone created [short names](querying.md#short-names).
