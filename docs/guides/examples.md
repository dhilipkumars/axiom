# Examples

Things worth doing that are awkward or impossible with `kubectl`.

These assume you have finished [Getting started](getting-started.md), so server
`prod` exists and its kinds are imported into schema `k8s`.

!!! note "Custom resources and writes may need a grant"

    The shipped RBAC in `deploy/k8s/gateway-rbac.yaml` reads broadly, covering
    workloads, nodes, networking, storage, events and metrics, so most examples
    below work as imported. It never reads Secrets, and it writes only
    ConfigMaps and the example CRD.

    A **custom resource** is readable if its operator ships an
    `aggregate-to-view` role. Otherwise, label a read-only ClusterRole for its
    API group `axiom.dhilipkumars.github.io/aggregate-to-gateway: "true"`
    ([Getting started](getting-started.md#what-the-gateway-can-see) shows one).
    To make a kind **writable**, add its verbs to the `axiom-gateway`
    ClusterRole. Neither needs a gateway restart; only revoking a grant does.
    Either way, re-import:

    ```sql
    -- foreign tables are catalog objects; they do not follow an RBAC change
    IMPORT FOREIGN SCHEMA k8s FROM SERVER prod INTO k8s;
    ```

## Reading

### Join across kinds

`kubectl` has no join. Which pods are on nodes reporting memory pressure:

```sql
SELECT p.namespace, p.name, n.name AS node
FROM k8s.core_pods p
JOIN k8s.core_nodes n ON n.name = p.node
WHERE n.status->'conditions' @> '[{"type":"MemoryPressure","status":"True"}]';
```

### Aggregate

Where are pods failing, and how:

```sql
SELECT namespace, phase, count(*)
FROM k8s.core_pods
WHERE phase <> 'Running'
GROUP BY namespace, phase
ORDER BY count(*) DESC;
```

### Filter on anything

`kubectl` gives label selectors and a few field selectors. SQL gives the whole
object — deployments that never finished rolling out:

```sql
SELECT namespace, name, replicas, ready_replicas
FROM k8s.apps_deployments
WHERE coalesce(ready_replicas, '0')::int < replicas::int;
```

Promoted columns are `text` on purpose, so the cast is yours and a missing
field is `NULL` rather than `0`. Anything no column promotes is still reachable
through `raw`:

```sql
SELECT namespace, name
FROM k8s.apps_deployments
WHERE raw->'spec'->'template'->'spec'->'containers' @> '[{"imagePullPolicy":"Always"}]';
```

### Find crash-looping pods

`CrashLoopBackOff` is **not** a pod phase — `phase` is `status.phase`, which is
only ever `Pending`, `Running`, `Succeeded`, `Failed` or `Unknown`. A
crash-looping pod is `Running`. The reason lives per container, which is
exactly the kind of nested field `kubectl` cannot filter on:

```sql
SELECT namespace, name
FROM k8s.core_pods
WHERE raw->'status'->'containerStatuses' @>
      '[{"state":{"waiting":{"reason":"CrashLoopBackOff"}}}]';
```

### Ask one question of several clusters

Each cluster is its own server and its own schema. This one needs a **second**
cluster and gateway — the walkthrough sets up one, imported into `k8s` — so
treat it as the shape rather than something to paste:

```sql
-- after CREATE SERVER stage ... and IMPORT ... INTO stage
SELECT 'prod' AS cluster, namespace, name FROM k8s.core_pods
WHERE raw->'status'->'containerStatuses' @> '[{"state":{"waiting":{"reason":"CrashLoopBackOff"}}}]'
UNION ALL
SELECT 'stage', namespace, name FROM stage.core_pods
WHERE raw->'status'->'containerStatuses' @> '[{"state":{"waiting":{"reason":"CrashLoopBackOff"}}}]';
```

## Writing

`INSERT`, `UPDATE` and `DELETE` are real API calls. A conflicting write raises
a SQL error rather than silently winning.

```sql
UPDATE k8s.core_configmaps
   SET data = data || '{"LOG_LEVEL":"debug"}'
 WHERE namespace = 'payments' AND name = 'api';

DELETE FROM k8s.core_configmaps WHERE namespace = 'staging' AND name = 'stale-flags';
```

**Pods are read-only at the SQL layer**, whatever RBAC allows. `DELETE FROM
k8s.core_pods` raises `0A000 foreign tables on pods are read-only` rather than
evicting anything — deleting a pod by `WHERE` clause is too easy to do by
accident and too hard to undo.

## Usage and events

Kubernetes forgets. Events expire after about an hour and usage is only ever
*now*, so the questions worth asking are the ones that combine both with live
state — which is also what a single `kubectl` invocation cannot do.

Two API groups reuse the core group's names. Every table is named for its
group, so core pods are `core_pods` and their usage is `metrics_k8s_io_pods`,
and events arrive as `core_events` and `events_k8s_io_events`.

### Why is that pod not running?

The pod's live state and the warning that explains it, on one row:

```sql
SELECT p.name, p.phase, e.reason #>> '{}' AS reason, e.message #>> '{}' AS message
  FROM k8s.core_events e
  JOIN k8s.core_pods p ON p.namespace = e.namespace
                      AND p.name = e.involved_object->>'name'
 WHERE e.type #>> '{}' = 'Warning'
   AND e.involved_object->>'kind' = 'Pod';
```

Event fields such as `type` and `reason` arrive as scalar `jsonb`, so
`#>> '{}'` unwraps them to text. `type = 'Warning'` does not compare text
with text; it tries to parse `Warning` as JSON and fails.

### Quantities are strings until you convert them

Kubernetes reports measurements as strings with unit suffixes — `49903n` of
CPU, `14488Ki` of memory. Postgres cannot compare or sum those, and `>` on
them is a string comparison that answers wrongly without erroring.
`axiom_quantity()` converts exactly:

```sql
SELECT axiom_quantity('100m'),   -- 0.100  cores
       axiom_quantity('128Mi'),  -- 134217728  bytes
       axiom_quantity('1M') = axiom_quantity('1Mi');  -- false: M is not Mi
```

It returns `NULL` rather than raising for anything that is not a quantity, so
one odd field cannot fail a query that spans a cluster.

### A capacity and risk review of the whole fleet

Per workload: what it actually consumes, what it reserved, whether it is
bounded at all, and whether anything is failing — usage, spec, the ownership
chain and events in one statement.

```sql
WITH usage AS (
  SELECT m.namespace, m.name AS pod,
         sum(axiom_quantity(c->'usage'->>'memory')) AS mem_used
    FROM k8s.metrics_k8s_io_pods m, jsonb_array_elements(m.containers) c
   GROUP BY 1, 2),
spec AS (
  SELECT p.namespace, p.name AS pod,
         p.metadata->'ownerReferences'->0->>'name' AS rs,
         sum(axiom_quantity(c->'resources'->'requests'->>'memory')) AS mem_req,
         bool_or(c->'resources'->'limits' IS NULL) AS no_limits
    FROM k8s.core_pods p, jsonb_array_elements(p.spec->'containers') c
   GROUP BY 1, 2, 3),
owner AS (
  SELECT r.namespace, r.name AS rs,
         coalesce(r.metadata->'ownerReferences'->0->>'name', r.name) AS workload
    FROM k8s.apps_replicasets r),
warn AS (
  SELECT e.namespace, e.involved_object->>'name' AS pod,
         count(*) AS warnings, max(e.reason #>> '{}') AS why
    FROM k8s.core_events e
   WHERE e.type #>> '{}' = 'Warning' AND e.involved_object->>'kind' = 'Pod'
   GROUP BY 1, 2)
SELECT coalesce(o.workload, s.pod) AS workload,
       count(*) AS pods,
       round(sum(u.mem_used) / 1024 / 1024) AS mem_used_mib,
       round(sum(s.mem_req) / 1024 / 1024) AS mem_requested_mib,
       CASE WHEN sum(s.mem_req) > 0
            THEN round(100 * sum(u.mem_used) / sum(s.mem_req)) END AS pct_of_request,
       bool_or(s.no_limits) AS unbounded,
       coalesce(sum(w.warnings), 0) AS warnings,
       max(w.why) AS latest_warning
  FROM spec s
  LEFT JOIN usage u ON u.namespace = s.namespace AND u.pod = s.pod
  LEFT JOIN owner o ON o.namespace = s.namespace AND o.rs = s.rs
  LEFT JOIN warn  w ON w.namespace = s.namespace AND w.pod = s.pod
 GROUP BY coalesce(o.workload, s.pod)
 ORDER BY mem_used_mib DESC NULLS LAST;
```

```
        workload        | pods | mem_used_mib | mem_requested_mib | pct_of_request | unbounded | warnings | latest_warning
------------------------+------+--------------+-------------------+----------------+-----------+----------+----------------
 api                    |   10 |           86 |                   |                | t         |        0 |
 web                    |    9 |           79 |                   |                | t         |        0 |
 axiom-gateway          |    1 |           14 |                64 |             22 | f         |        0 |
 broken                 |    1 |              |                   |                | t         |        3 | Failed
```

`api` and `web` consume 165 MiB between them while requesting nothing and
capping nothing — invisible to the scheduler, unbounded at runtime. The
gateway reserves 64 MiB and uses 14. `broken` is not running, and why is on
the same row.

**Keep `usage` on a `LEFT JOIN`.** An inner join drops exactly the workloads
that have no metrics because they never started — the ones you most want to
see.

### Find it and fix it in one statement

The review above produces a list. A write turns it into an action: annotate
every Deployment that uses under a fifth of the memory it requests, so the
finding sits on the object itself where `kubectl describe` and every other tool
will see it.

The shipped RBAC grants writes per resource and not this one, so grant it
first:

```sh
kubectl patch clusterrole axiom-gateway --type=json -p='[{"op":"add","path":"/rules/-","value":
  {"apiGroups":["apps"],"resources":["deployments"],"verbs":["get","list","watch","update"]}}]'
```

```sql
WITH used AS (
  SELECT m.namespace, m.name AS pod, sum(axiom_quantity(c->'usage'->>'memory')) AS bytes
    FROM k8s.metrics_k8s_io_pods m, jsonb_array_elements(m.containers) c
   GROUP BY 1, 2),
requested AS (
  SELECT p.namespace, p.name AS pod,
         r.metadata->'ownerReferences'->0->>'name' AS deployment,
         sum(axiom_quantity(c->'resources'->'requests'->>'memory')) AS bytes
    FROM k8s.core_pods p
    JOIN k8s.apps_replicasets r
      ON r.namespace = p.namespace AND r.name = p.metadata->'ownerReferences'->0->>'name',
         jsonb_array_elements(p.spec->'containers') c
   GROUP BY 1, 2, 3),
ratio AS (
  SELECT q.namespace, q.deployment, round(100 * sum(u.bytes) / sum(q.bytes)) AS pct
    FROM requested q JOIN used u USING (namespace, pod)
   GROUP BY 1, 2
  HAVING sum(q.bytes) > 0)
UPDATE k8s.apps_deployments d
   SET annotations = coalesce(d.annotations, '{}')
                     || jsonb_build_object('axiom/memory-used-pct', r.pct::text)
  FROM ratio r
 WHERE d.namespace = r.namespace AND d.name = r.deployment AND r.pct < 20
RETURNING d.namespace, d.name, r.pct;
```

```sh
kubectl get deploy lean -o jsonpath='{.metadata.annotations.axiom/memory-used-pct}'
```

Each row is a real Kubernetes update, carrying the `resourceVersion` it read,
so a Deployment that changed in the meantime fails the statement with `40001`
rather than being overwritten. Annotating changes no pod template, so nothing
rolls. Rewriting `resources.requests` instead would right-size the workload in
the same statement, and would also restart every pod it touched. That is why
the example annotates.

### Join the cluster to your own data

Axiom runs inside your application's Postgres, so live cluster state joins to
your business tables directly. There is no export and no copy going stale. With
a table mapping namespaces to customers:

```sql
CREATE TABLE tenants (namespace text PRIMARY KEY, customer text, plan text);
```

Which customers are hit by a failing pod right now, and why:

```sql
SELECT t.customer, t.plan, p.name AS pod, e.reason #>> '{}' AS reason
  FROM tenants t
  JOIN k8s.core_pods p ON p.namespace = t.namespace
  JOIN k8s.core_events e ON e.namespace = p.namespace
                        AND e.involved_object->>'name' = p.name
 WHERE e.type #>> '{}' = 'Warning'
   AND e.involved_object->>'kind' = 'Pod';
```

Memory in use per customer, which is the start of a chargeback report:

```sql
SELECT t.customer,
       round(sum(axiom_quantity(c->'usage'->>'memory')) / 1024 / 1024) AS mem_mib
  FROM tenants t
  JOIN k8s.metrics_k8s_io_pods m ON m.namespace = t.namespace,
       jsonb_array_elements(m.containers) c
 GROUP BY t.customer
 ORDER BY mem_mib DESC;
```

### Requirements

Usage tables need [metrics-server][ms] installed in the cluster; without it
`metrics.k8s.io` does not exist and the tables are simply absent. The gateway
also needs `list` on that group. The shipped RBAC grants it, so metrics-server
installed later is picked up on the next discovery refresh with no RBAC edit.
Custom and external metrics APIs, from an adapter such as KEDA or
prometheus-adapter, are granted the same way.

[ms]: https://github.com/kubernetes-sigs/metrics-server

## Manage Postgres with Postgres

The clearest demonstration of what Axiom is: one Postgres instance creating,
inspecting and scaling another — through [CloudNativePG](https://cloudnative-pg.io),
with no `kubectl` and no YAML.

**First, install the operator**, which is what creates the CRD the gateway can
then discover:

```sh
kubectl apply --server-side -f \
  https://raw.githubusercontent.com/cloudnative-pg/cloudnative-pg/release-1.30/releases/cnpg-1.30.0.yaml

kubectl wait --for=condition=Available deploy/cnpg-controller-manager \
  -n cnpg-system --timeout=240s
```

**Then let the gateway write the kind.** Reads may already be covered, but the
shipped RBAC grants writes only per resource, and these examples create and
scale clusters:

```sh
kubectl patch clusterrole axiom-gateway --type=json -p='[{"op":"add","path":"/rules/-","value":
  {"apiGroups":["postgresql.cnpg.io"],"resources":["clusters"],
   "verbs":["get","list","watch","create","update","delete"]}}]'
```

The gateway sees a new grant on the next import, with no restart. Foreign
tables are catalog objects and do not follow an RBAC change, so import the
kind now:

```sql
IMPORT FOREIGN SCHEMA k8s LIMIT TO (postgresql_cnpg_io_clusters) FROM SERVER prod INTO k8s;
```

That produces the usual shape — the universal columns, the kind's own top-level
fields as `jsonb`, and `raw`:

```
       Column       | Type
--------------------+-------
 api_version        | text
 kind               | text
 name               | text
 namespace          | text
 ...
 spec               | jsonb
 status             | jsonb
 raw                | jsonb

FDW options: (resource 'clusters', "group" 'postgresql.cnpg.io', version 'v1', kind 'Cluster')
```

Now create one:

```sql
INSERT INTO k8s.postgresql_cnpg_io_clusters (namespace, name, spec) VALUES (
  'default', 'demo-db',
  '{"instances": 1, "storage": {"size": "256Mi"}}'::jsonb
);
```

```
INSERT 0 1
```

It is a real object, and the operator picks it up:

```sh
$ kubectl get cluster.postgresql.cnpg.io -n default
NAME      AGE   INSTANCES   READY   STATUS   PRIMARY
demo-db   0s
```

Watch it come up without leaving SQL:

```sql
SELECT name, status->>'phase' AS phase, status->>'readyInstances' AS ready
FROM k8s.postgresql_cnpg_io_clusters WHERE namespace = 'default';
```

### Scale it with an UPDATE

`spec` is a `jsonb` column, so changing the cluster is a `jsonb_set`:

```sql
UPDATE k8s.postgresql_cnpg_io_clusters
   SET spec = jsonb_set(spec, '{instances}', '3')
 WHERE namespace = 'default' AND name = 'demo-db';
```

```
UPDATE 1
```

The operator reconciles, and the change is visible from either side:

```sh
$ kubectl get cluster.postgresql.cnpg.io demo-db -o jsonpath='{.spec.instances}'
3
```

A conflicting write does not silently win: Axiom sends the `resourceVersion` it
read, so if something else changed the object first the API server rejects the
update and you get a SQL error rather than a lost write.

Nothing here is CNPG-specific. Any CRD the gateway's RBAC permits becomes a
table the same way, with `spec` as the field you write.
