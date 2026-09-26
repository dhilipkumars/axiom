# Querying

## What is pushed down

Axiom pushes to the API server what the API server can actually filter on, and
nothing else. That is a short list, because the Kubernetes API is not a query
engine:

- **namespace**, as the request's namespace scope;
- **name**, as a `metadata.name` field selector.

Everything else is a local filter. A `WHERE phase = 'Running'` fetches the
namespace and filters in Postgres, because the API server has no general
predicate language to hand it to.

This is visible in the gateway's log, which records the filters each `List`
received:

```sh
kubectl -n axiom-system logs deploy/axiom-gateway | grep '"msg":"list"'
```

A scan with both quals logs the namespace and the name. A scan with neither
lists cluster-wide, which on a large cluster is exactly as expensive as it
sounds. Scope your queries by namespace where you can.

`EXPLAIN` plans without contacting the gateway at all: a foreign table's
generated DDL carries the kind's fully resolved identity, so planning needs no
discovery.

## Table names

`IMPORT FOREIGN SCHEMA` names every table **`<group>_<plural>`**, with the
core API group spelled `core`:

| API group | Resource | Table |
|---|---|---|
| (core) | `pods` | `core_pods` |
| `apps` | `deployments` | `apps_deployments` |
| `events.k8s.io` | `events` | `events_k8s_io_events` |
| `metrics.k8s.io` | `pods` | `metrics_k8s_io_pods` |
| `postgresql.cnpg.io` | `clusters` | `postgresql_cnpg_io_clusters` |

A name depends on its own group and resource and nothing else, so it does not
change when something else is installed in the cluster. A plural is only
unique within its group, and real clusters reuse them: metrics-server serves
`pods` and `nodes` beside the core ones, and CloudNativePG and Cluster API both
define `clusters`. If a name depended on what else was present, installing any
of those would rename a table your queries and views depend on.

In a group name `.` becomes `_` and `-` becomes `__`, so two groups can never
produce the same name. A name that would exceed Postgres's 63-byte identifier
limit keeps its resource whole and shortens the group, with a digest of the
full name between them:
`dbforpostg_aeb7bb51_flexibleserveractivedirectoryadministrators`.

**`LIMIT TO` and `EXCEPT` take these table names**, because Postgres matches
them against the tables an import generates. Naming a resource instead
imports nothing, with a `WARNING` suggesting the table you probably meant:

```sql
IMPORT FOREIGN SCHEMA k8s LIMIT TO (core_pods, apps_deployments) FROM SERVER prod INTO k8s;
```

### Short names

If you would rather type `pods`, ask for short names. Each one is a view over
the qualified table:

```sql
SELECT * FROM axiom_create_short_names('k8s');

 short_name  |      target      | status
-------------+------------------+---------
 deployments | apps_deployments | created
 pods        | core_pods        | created
```

- **The core group gets the bare plural.** `pods` means `core_pods` even with
  `metrics_k8s_io_pods` beside it, so running this again gives the same answer.
- **Any other shared plural is skipped and reported**, never guessed. Choose
  one yourself:
  `SELECT axiom_create_short_name('k8s', 'clusters', 'postgresql_cnpg_io_clusters');`
- **An existing name is never repointed or replaced.** A short name keeps
  pointing where it was created to point, and an object you created yourself
  is left alone.
- With two servers in one schema (through the `prefix` import option), their
  core kinds share a plural too. Pass `server_name => 'prod'` to choose.

Reads and writes through a short name behave exactly as they do on the table.
Each view uses `security_invoker`, so it checks the caller's privileges, not
its creator's. A role therefore needs its grant on both the view and the table
behind it: `GRANT SELECT ON k8s.pods, k8s.core_pods TO app;`.

A view does not survive dropping the table under it. Refreshing an import
means `DROP ... CASCADE` and importing again, so call
`axiom_create_short_names` again afterwards.

## Columns

Every table has the same universal columns, then the kind's own top-level
fields, then `raw`. The [column reference](../generated/columns.md) has the
full rules; the parts that matter in practice:

**Everything is `text` or `jsonb`.** Numbers come back as text, so a numeric
comparison is an explicit cast:

```sql
SELECT name FROM k8s.apps_deployments
 WHERE ready_replicas::int < replicas::int;
```

That is deliberate. An OpenAPI schema often does not constrain a field tightly
enough to justify a numeric column, and a wrong guess turns the table into a
cast-error minefield. It also makes an absent field NULL rather than zero,
which is usually what you want.

**`raw` always holds the whole object**, and is how to reach anything no column
promotes:

```sql
SELECT name, raw->'spec'->'containers'->0->>'image' FROM k8s.core_pods
 WHERE namespace = 'default';
```

**Field names are normalised.** A `camelCase` Kubernetes field becomes
`snake_case`, so `nodeName` is `node_name`. Two fields that normalise to the
same name produce no column at all rather than an arbitrary winner.

**`api_version`, `kind` and `metadata` are on every kind**, which is what makes
a query spanning kinds possible:

```sql
SELECT kind, name, namespace FROM k8s.core_pods
UNION ALL
SELECT kind, name, namespace FROM k8s.core_configmaps
ORDER BY kind, name;
```

## Writing

`INSERT`, `UPDATE` and `DELETE` map to the API server's create, update and
delete. Writes are never served from cache and never batched: each statement is
one RPC.

```sql
INSERT INTO k8s.core_configmaps (name, namespace, data)
  VALUES ('app', 'default', '{"LOG_LEVEL":"info"}');

UPDATE k8s.core_configmaps SET data = data || '{"LOG_LEVEL":"debug"}'
 WHERE namespace = 'default' AND name = 'app';
```

**`raw` is a whole object on INSERT too.** A complete manifest can be inserted
as one `jsonb` value, and a typed column given alongside it overrides the same
field, so `raw` read from one object works as a template for another:

```sql
INSERT INTO k8s.core_configmaps (namespace, name, raw) VALUES ('default', 'app',
  '{"metadata":{"labels":{"team":"payments"}},"data":{"LOG_LEVEL":"info"}}');

-- a copy of it under a new name, with data overridden
INSERT INTO k8s.core_configmaps (namespace, name, data, raw)
  SELECT 'default', 'app-copy', '{"LOG_LEVEL":"debug"}', raw
    FROM k8s.core_configmaps WHERE namespace = 'default' AND name = 'app';
```

A column the INSERT leaves NULL takes its value from `raw`. Metadata the API
server assigns at creation (`uid`, `resourceVersion`, `creationTimestamp`,
`managedFields` and the like) is dropped from `raw` rather than sent, and a
`raw` whose `apiVersion` or `kind` names a different kind from the table is
refused rather than relabelled.

**Conflicts are retryable.** An `UPDATE` carries the `resource_version` the row
was read at. If the object changed in between, the statement fails with
`40001`, the same SQLSTATE Postgres uses for a serialization failure, and the
fix is the usual one: re-read and retry.

**Identity and server-managed fields are refused.** Changing `name`,
`namespace`, `uid` or `resource_version` raises `0A000` rather than silently
doing something surprising.

**Some kinds are read-only** regardless of RBAC. Pods are the clearest case: a
SQL `UPDATE` of a Pod has no sane meaning, so the extension keeps it read-only
and refuses `writable 'true'` on such a table.

## Caching

**Axiom requires the extension to be preloaded** — not just caching. The cache
lives in shared memory and is filled by a background worker, neither of which
can be set up after startup, so `CREATE EXTENSION axiom` fails outright without
it:

```
# postgresql.conf, then restart -- this cannot change at runtime
shared_preload_libraries = 'axiom'
```

**Append to the existing list rather than replacing it.** The setting is one
comma-separated list, so `shared_preload_libraries = 'pg_stat_statements,axiom'`
if something is already there. Copying the line above over a non-empty setting
silently disables whatever it replaced, at the next restart.

Without it, installing the extension fails and says what to do:

```
ERROR:  axiom must be loaded through shared_preload_libraries
DETAIL:  Add `shared_preload_libraries = 'axiom'` to postgresql.conf, restart
Postgres, then run CREATE EXTENSION axiom. ...
```

The published Postgres images set this themselves, so nothing above applies if
you are using one. Two settings are worth knowing alongside it, both also fixed
at startup:

| Setting | Default | What it does |
| --- | --- | --- |
| `axiom.cache_size_mb` | `256` | Upper bound of the shared-memory cache. On reaching it, affected subscriptions stop taking objects rather than evicting — `DEGRADED` if they had synced, `REQUESTED` if they had not. What a scan does then is below. |
| `axiom.notify_database` | `postgres` | Database the worker opens for `NOTIFY axiom_events`. `LISTEN` there, which is not necessarily the database you query from. |

By default (`cache_mode 'on_demand'`) every scan is an RPC. A table declared
with `cache_mode 'watch'` is served instead from a shared-memory cache kept current by a watch stream:

```sql
CREATE FOREIGN TABLE k8s_pods_live (
  name text, namespace text, phase text, node text, raw jsonb
) SERVER prod OPTIONS (resource 'pods', cache_mode 'watch');
```

The first scan is served on demand while the subscription starts. After that,
scans do not reach the gateway at all, which `axiom_gateway_stats()` will show
as a `list_calls` that stops moving.

A subscription is in one of three states, readable from SQL:

```sql
SELECT * FROM axiom_watch_status();
```

Only superusers can call it by default, because it names every watched
resource and namespace whatever the caller's table grants. Grant it to the
roles that monitor Axiom:
`GRANT EXECUTE ON FUNCTION axiom_watch_status() TO monitoring;`.

- **`ACTIVE`** — the stream is current and scans are authoritative.
- **`DEGRADED`** — the stream is broken and the cache is stale. Scans still
  return the cached rows, with a `WARNING` saying so on every scan. Stale data
  is offered, never silently.
- **`RESYNCING`** — the stream is being re-established.

Two of the columns are worth watching over time. `objects` is what a scan will
return. `tombstones` is objects deleted in the cluster that the cache still
holds briefly, so that a scan running at the moment of a delete does not watch
a row vanish. They are never returned, and a sweep clears them a couple of
seconds later. A count that keeps climbing rather than returning to zero means
sweeping is not keeping up, and that memory is not being reclaimed.

**When the cache fills**, `axiom_watch_status()` reports a reason naming
`axiom.cache_size_mb`, and what happens to your queries depends on when it
filled.

If it filled while an established watch was running, the cache is a complete
snapshot that has stopped taking changes, so it keeps being served — stale, and
every scan says so. If it filled while the cache was still being built, there
is no complete snapshot to serve: an incomplete listing has no way to know
which rows it is missing, so scans of that table fall back to the gateway
instead. Queries keep working either way.

Retrying is deliberately slow — a minute between attempts rather than climbing
from a second — because nothing about reconnecting frees space. A subscription
that had synced resumes from its bookmark and replays, which is cheap but
equally futile; one that had not repeats a full listing of the collection every
time, which is not cheap at all.

It does keep trying, and a sweep reclaiming expired tombstones is the one way
room appears without intervention. If that does not free enough, raise
`axiom.cache_size_mb` and restart the server — a subscription slot is never
released once taken, so waiting for another table to give its cache back is not
something to count on.

Recovery resumes from the last bookmark rather than relisting, so a gateway
restart does not re-fetch every object. The trade is simple: caching means
never paying for a scan, at the cost of a window where the rows are stale and
you are told about it.

Use `cache_mode 'watch'` for kinds you read repeatedly and whose staleness you
can tolerate for seconds. Leave it off for the read-once queries, and for
anything where a stale answer is worse than a slow one.
