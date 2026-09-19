# Examples

Things worth doing that are awkward or impossible with `kubectl`. Every query
here was run against a live cluster; the CNPG example at the end was verified
end to end, including that `kubectl` saw the object Postgres created.

These assume you have finished [Getting started](getting-started.md), so a
schema is imported and `k8s.*` tables exist.

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

### Ask one question of several clusters

Each cluster is a server, each server a schema:

```sql
SELECT 'prod' AS cluster, name, phase FROM prod.pods  WHERE phase = 'CrashLoopBackOff'
UNION ALL
SELECT 'stage',           name, phase FROM stage.pods WHERE phase = 'CrashLoopBackOff';
```

## Writing

`INSERT`, `UPDATE` and `DELETE` are real API calls. A conflicting write raises
a SQL error rather than silently winning.

```sql
UPDATE k8s.configmaps
   SET data = data || '{"LOG_LEVEL":"debug"}'
 WHERE namespace = 'payments' AND name = 'api';

DELETE FROM k8s.pods WHERE namespace = 'staging' AND phase = 'Failed';
```

## Creating a Postgres cluster, from Postgres

The clearest demonstration of what Axiom is: a Postgres instance provisioning a
Postgres cluster by `INSERT`, through [CloudNativePG](https://cloudnative-pg.io).

**First, let the gateway see the kind.** What a query can reach is bounded by
the gateway's RBAC, and the shipped ClusterRole does not grant CNPG — nothing
does by default, which is the point:

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
IMPORT FOREIGN SCHEMA k8s LIMIT TO (clusters) FROM SERVER k8s INTO k8s;
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
