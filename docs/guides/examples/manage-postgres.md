# Manage Postgres with Postgres

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

## Scale it with an UPDATE

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

