# Changing the cluster

`INSERT`, `UPDATE` and `DELETE` are real API calls: each row is one create,
update or delete, sent as the gateway's identity and checked by the API server
like any other client's. This page shows them working, with output from a CI run against the `shop`
namespace the [Examples](index.md) use.

## Change a ConfigMap

```sql
--8<-- "docs/snippets/examples/write-configmap.sql"
```

```
      name       |                   data                    
-----------------+-------------------------------------------
 checkout-config | {"CURRENCY": "EUR", "LOG_LEVEL": "debug"}
(1 row)
```

`kubectl` sees the change at once, because it is in the cluster, not in
Postgres:

```sh
$ kubectl -n shop get configmap checkout-config -o jsonpath='{.data.LOG_LEVEL}'
debug
```

## Insert a whole manifest

**`raw` is a whole object on INSERT too.** A complete manifest can be inserted
as one `jsonb` value, and a typed column given beside it overrides the same
field, so `raw` read from one object is a template for another:

```sql
--8<-- "docs/snippets/examples/insert-raw.sql"
```

```
 name  |        labels        |          data           
-------+----------------------+-------------------------
 flags | {"team": "payments"} | {"NEW_CHECKOUT": "off"}
(1 row)

     name     |        labels        |          data          
--------------+----------------------+------------------------
 flags-canary | {"team": "payments"} | {"NEW_CHECKOUT": "on"}
(1 row)
```

A column the INSERT leaves NULL takes its value from `raw`. Metadata the API
server assigns (`uid`, `resourceVersion`, `creationTimestamp`, `managedFields`)
is dropped from `raw` rather than sent, and a `raw` whose `apiVersion` or `kind`
names another kind is refused rather than relabelled.

Writes are never served from cache and never batched: each row is one call.

## What Axiom refuses

**Pods are read-only at the SQL layer, whatever RBAC allows.** Deleting a pod
by `WHERE` clause is too easy to do by accident and too hard to undo:

```sql
--8<-- "docs/snippets/examples/pods-read-only.sql"
```

```
psql:<stdin>:1: ERROR:  foreign table "core_pods" does not allow deletes
```

**A conflicting write does not silently win.** An `UPDATE` carries the
`resourceVersion` of the row it read. If something else changed the object in
between, the API server rejects it and the statement fails with `40001`, the
SQLSTATE Postgres itself uses for a serialization failure: re-read and retry.

**Identity and server-managed fields are refused.** Changing `name`,
`namespace`, `uid` or `resource_version` raises `0A000` rather than doing
something surprising.

## Find it and fix it in one statement

The shipped RBAC grants writes per resource, and not on Deployments, so grant
that first:

```sh
kubectl patch clusterrole axiom-gateway --type=json -p='[{"op":"add","path":"/rules/-","value":
  {"apiGroups":["apps"],"resources":["deployments"],"verbs":["get","list","watch","update"]}}]'
```

Then annotate each Deployment in `shop` using under a fifth of the memory it
requests. It is scoped to one namespace on purpose: drop that line and it
annotates `kube-system` too.
It reads live usage and live spec, walks pod → ReplicaSet → Deployment, and
writes the answer back:

```sql
--8<-- "docs/snippets/examples/find-and-fix.sql"
```

```
 namespace |  name   | pct 
-----------+---------+-----
 shop      | catalog |   0
 shop      | web     |   1
(2 rows)
```

```sh
$ kubectl -n shop get deploy catalog -o jsonpath='{.metadata.annotations.axiom/memory-used-pct}'
0
```

Annotating changes no pod template, so nothing rolls. Rewriting
`resources.requests` instead would right-size the workload in the same
statement, and would also restart every pod it touched, which is why the
example annotates.
