# How tables work

What a query against an Axiom table actually does: what reaches the API
server, how a column gets its type, and when a scan is served from cache.

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

## Columns

Every table has the same universal columns, then the kind's own top-level
fields, then `raw`. The [column reference](../../generated/columns.md) has the
full rules; the parts that matter in practice:

**Columns have the type the kind's schema gives them.** A field whose OpenAPI
schema names one scalar type is `text`, `bigint`, `boolean` or `timestamptz`;
anything else is `jsonb`. So the queries you would write just work:

```sql
SELECT namespace, name, reason, count FROM k8s.core_events
 WHERE type = 'Warning' AND last_timestamp > now() - interval '1 hour'
 ORDER BY count DESC;

SELECT name FROM k8s.apps_deployments WHERE ready_replicas < replicas;
```

An absent field is NULL rather than zero, which is usually what you want. Fields
that can hold more than one type, such as a quantity (`500m`) or a `maxSurge`
(`25%` or `2`), stay `jsonb`; compare quantities with `axiom_quantity()`. The
[column reference](../../generated/columns.md) has the exact rule.

**The declared type decides how a column reads**, so one column can be
changed in place:

```sql
ALTER FOREIGN TABLE k8s.core_events ALTER COLUMN count TYPE bigint;
```

A query written for the old types fails loudly after a re-import rather than
answering differently: `type->>0` on a `text` column is `operator does not
exist`. Write `type = 'Warning'` instead.

A kind's own top-level field can be declared `jsonb`, `text`, `bigint`, `boolean`
or `timestamptz`. `creation_timestamp` can be `timestamptz` or `text`, and a
Deployment's replica counts `bigint` or `text`. Any other declaration is refused
when the table is first queried, with the types the column accepts.

Postgres will not change the type of a column a view uses, which includes the
short-name views `axiom_create_short_names` makes: drop the view, alter the
table, and create the view again.

The conversion is strict. `text` reads a JSON string, `bigint` a JSON integer,
`boolean` a JSON boolean, and `timestamptz` an RFC 3339 string such as
`2024-05-01T10:00:00Z`. A value that is not of the declared type reads as NULL,
not as an error, so one malformed object cannot break every query on its table.
Writes go the other way: `SET count = 5` writes the JSON number `5`, and
`SET immutable = true` the boolean `true`. A timestamp is written in UTC with six
fractional digits, which every Kubernetes timestamp field accepts, and an
UPDATE leaves the fields it did not change exactly as they were.

**`raw` always holds the whole object**, and is how to reach anything no column
promotes:

```sql
SELECT name, raw->'spec'->'containers'->0->>'image' FROM k8s.core_pods
 WHERE namespace = 'default';
```

**Field names are normalised.** A `camelCase` Kubernetes field becomes
`snake_case`, so `nodeName` is `node_name`. Two fields that normalise to the
same name produce no column at all rather than an arbitrary winner. The
column's `field` option records the real spelling, `string_data jsonb OPTIONS
(field 'stringData')`, so an INSERT writes `stringData`. A hand-written table
needs the option on any column whose field is not spelled like the column:
`ALTER FOREIGN TABLE k8s.core_secrets ALTER COLUMN string_data OPTIONS (ADD
field 'stringData')`.

**`api_version`, `kind` and `metadata` are on every kind**, which is what makes
a query spanning kinds possible:

```sql
SELECT kind, name, namespace FROM k8s.core_pods
UNION ALL
SELECT kind, name, namespace FROM k8s.core_configmaps
ORDER BY kind, name;
```

## Caching

Caching needs Axiom preloaded, which every install route does
([packages](../install/packages.md#2-preload-it-and-restart)). The cache's size
is `axiom.cache_size_mb`, 256 MB by default, fixed at startup.

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
