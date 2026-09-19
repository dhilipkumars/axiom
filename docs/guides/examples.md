# Examples

Things worth doing that are awkward or impossible with `kubectl`.

The CloudNativePG example at the end was run end to end, including confirming
that `kubectl` saw the object Postgres created. The read queries are written
against the column reference in [Foreign table
columns](../generated/columns.md) rather than run individually — if one does
not work, that is a bug worth reporting.

These assume you have finished [Getting started](getting-started.md), so server
`prod` exists and its kinds are imported into schema `k8s`.

!!! warning "Most of these need a kind the shipped RBAC does not grant"

    The ClusterRole in `deploy/k8s/gateway-rbac.yaml` grants **pods**,
    **configmaps** and the example CRD — nothing else. That is the design, not
    an oversight: what a query can reach is bounded by the gateway's
    ServiceAccount, and there is deliberately no wildcard.

    Examples below that use `nodes`, `deployments` or a CRD need that kind
    granted first: grant, restart, re-import.

    **The API group differs per kind**, and getting it wrong fails silently —
    the gateway's access check rejects the kind, `IMPORT FOREIGN SCHEMA` simply
    omits it, and the query then says the relation does not exist:

    | Kind | `apiGroups` |
    |---|---|
    | `nodes`, `pods`, `configmaps` | `[""]` — the core group |
    | `deployments` | `["apps"]` |
    | CloudNativePG `clusters` | `["postgresql.cnpg.io"]` |

    ```sh
    # nodes: core group, so apiGroups is the empty string
    kubectl patch clusterrole axiom-gateway --type=json -p='[{"op":"add","path":"/rules/-","value":
      {"apiGroups":[""],"resources":["nodes"],"verbs":["get","list","watch"]}}]'

    # deployments: apps group
    kubectl patch clusterrole axiom-gateway --type=json -p='[{"op":"add","path":"/rules/-","value":
      {"apiGroups":["apps"],"resources":["deployments"],"verbs":["get","list","watch"]}}]'

    kubectl -n axiom-system rollout restart deploy/axiom-gateway
    kubectl -n axiom-system rollout status deploy/axiom-gateway
    ```

    ```sql
    -- foreign tables are catalog objects; they do not follow an RBAC change
    IMPORT FOREIGN SCHEMA k8s LIMIT TO (nodes, deployments) FROM SERVER prod INTO k8s;
    ```

## Reading

### Join across kinds

`kubectl` has no join. Which pods are on nodes reporting memory pressure:

```sql
SELECT p.namespace, p.name, n.name AS node
FROM k8s.pods p
JOIN k8s.nodes n ON n.name = p.node
WHERE n.status->'conditions' @> '[{"type":"MemoryPressure","status":"True"}]';
```

### Aggregate

Where are pods failing, and how:

```sql
SELECT namespace, phase, count(*)
FROM k8s.pods
WHERE phase <> 'Running'
GROUP BY namespace, phase
ORDER BY count(*) DESC;
```

### Filter on anything

`kubectl` gives label selectors and a few field selectors. SQL gives the whole
object — deployments that never finished rolling out:

```sql
SELECT namespace, name, replicas, ready_replicas
FROM k8s.deployments
WHERE coalesce(ready_replicas, '0')::int < replicas::int;
```

Promoted columns are `text` on purpose, so the cast is yours and a missing
field is `NULL` rather than `0`. Anything no column promotes is still reachable
through `raw`:

```sql
SELECT namespace, name
FROM k8s.deployments
WHERE raw->'spec'->'template'->'spec'->'containers' @> '[{"imagePullPolicy":"Always"}]';
```

### Find crash-looping pods

`CrashLoopBackOff` is **not** a pod phase — `phase` is `status.phase`, which is
only ever `Pending`, `Running`, `Succeeded`, `Failed` or `Unknown`. A
crash-looping pod is `Running`. The reason lives per container, which is
exactly the kind of nested field `kubectl` cannot filter on:

```sql
SELECT namespace, name
FROM k8s.pods
WHERE raw->'status'->'containerStatuses' @>
      '[{"state":{"waiting":{"reason":"CrashLoopBackOff"}}}]';
```

### Ask one question of several clusters

Each cluster is its own server and its own schema. This one needs a **second**
cluster and gateway — the walkthrough sets up one, imported into `k8s` — so
treat it as the shape rather than something to paste:

```sql
-- after CREATE SERVER stage ... and IMPORT ... INTO stage
SELECT 'prod' AS cluster, namespace, name FROM k8s.pods
WHERE raw->'status'->'containerStatuses' @> '[{"state":{"waiting":{"reason":"CrashLoopBackOff"}}}]'
UNION ALL
SELECT 'stage', namespace, name FROM stage.pods
WHERE raw->'status'->'containerStatuses' @> '[{"state":{"waiting":{"reason":"CrashLoopBackOff"}}}]';
```

## Writing

`INSERT`, `UPDATE` and `DELETE` are real API calls. A conflicting write raises
a SQL error rather than silently winning.

```sql
UPDATE k8s.configmaps
   SET data = data || '{"LOG_LEVEL":"debug"}'
 WHERE namespace = 'payments' AND name = 'api';

DELETE FROM k8s.configmaps WHERE namespace = 'staging' AND name = 'stale-flags';
```

**Pods are read-only at the SQL layer**, whatever RBAC allows. `DELETE FROM
k8s.pods` raises `0A000 foreign tables on pods are read-only` rather than
evicting anything — deleting a pod by `WHERE` clause is too easy to do by
accident and too hard to undo.

## Creating a Postgres cluster, from Postgres

The clearest demonstration of what Axiom is: a Postgres instance provisioning a
Postgres cluster by `INSERT`, through [CloudNativePG](https://cloudnative-pg.io).

**First, install the operator**, which is what creates the CRD the gateway can
then discover:

```sh
kubectl apply --server-side -f \
  https://raw.githubusercontent.com/cloudnative-pg/cloudnative-pg/release-1.30/releases/cnpg-1.30.0.yaml

kubectl wait --for=condition=Available deploy/cnpg-controller-manager \
  -n cnpg-system --timeout=240s
```

**Then let the gateway see the kind.** What a query can reach is bounded by the
gateway's RBAC, and nothing grants CNPG by default:

```sh
kubectl patch clusterrole axiom-gateway --type=json -p='[{"op":"add","path":"/rules/-","value":
  {"apiGroups":["postgresql.cnpg.io"],"resources":["clusters"],
   "verbs":["get","list","watch","create","update","delete"]}}]'

kubectl -n axiom-system rollout restart deploy/axiom-gateway
kubectl -n axiom-system rollout status deploy/axiom-gateway
```

Foreign tables are catalog objects and do not follow an RBAC change, so import
the kind after restarting:

```sql
IMPORT FOREIGN SCHEMA k8s LIMIT TO (clusters) FROM SERVER prod INTO k8s;
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
INSERT INTO k8s.clusters (namespace, name, spec) VALUES (
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

And you can watch it come up without leaving SQL:

```sql
SELECT name, status->>'phase' AS phase, status->>'readyInstances' AS ready
FROM k8s.clusters WHERE namespace = 'default';
```

Nothing here is CNPG-specific. Any CRD the gateway's RBAC permits becomes a
table the same way, with `spec` as the field you write.
