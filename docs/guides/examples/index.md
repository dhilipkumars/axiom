---
description: "SQL examples for Kubernetes with Axiom: crash-looping pods, stuck rollouts, failing pods joined to your own data, and fixing them in one statement."
---

# Examples

Questions `kubectl` cannot answer in one command, answered in one query.

Every query here runs in CI against a small `shop` namespace with a few things
wrong in it, and the output under each is from that run. They assume you have
finished [Initialize](../initialize.md), so server `prod` exists and
its kinds are imported into schema `k8s`.

## Which pods keep restarting?

`CrashLoopBackOff` is not a pod phase: a crash-looping pod is `Running`. It is
not even a steady state of the container, which alternates between
`CrashLoopBackOff` and the error from its last attempt, so filtering on it
misses the pod half the time. The restart count only grows, and it lives on
each container, a nested field `kubectl` cannot filter on.

```sql
--8<-- "docs/snippets/examples/crash-looping.sql"
```

```
 namespace |       pod       |  phase  | container | restarts |        why        
-----------+-----------------+---------+-----------+----------+-------------------
 shop      | checkout-worker | Running | worker    |        3 | RunContainerError
(1 row)
```

## Why is that pod not running?

The pod's state and the latest warning that explains it, on one row.
`kubectl get pods` and `kubectl get events` are two commands and a squint.

```sql
--8<-- "docs/snippets/examples/not-running.sql"
```

```
 namespace |            pod            |  phase  |      reason      |                                                                                                                                                                      message                                                                                                                                                                      
-----------+---------------------------+---------+------------------+---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------
 shop      | checkout-6b889d49cf-jt7bz | Pending | FailedScheduling | 0/1 nodes are available: 1 Insufficient cpu. no new claims to deallocate, preemption: 0/1 nodes are available: 1 Preemption is not helpful for scheduling.
 shop      | report                    | Pending | Failed           | Failed to pull image "registry.invalid/shop/report:1.4": failed to pull and unpack image "registry.invalid/shop/report:1.4": failed to resolve reference "registry.invalid/shop/report:1.4": failed to do request: Head "https://registry.invalid/v2/shop/report/manifests/1.4": dial tcp: lookup registry.invalid on 172.18.0.1:53: no such host
(2 rows)
```

## Which rollouts are stuck, and why?

Four kinds in one statement: each Deployment short of its replicas, through its
ReplicaSet to the pod that will not start, and the event that says why.

```sql
--8<-- "docs/snippets/examples/stuck-rollouts.sql"
```

```
 namespace | deployment | ready |            pod            |                                                                            why                                                                             
-----------+------------+-------+---------------------------+------------------------------------------------------------------------------------------------------------------------------------------------------------
 shop      | checkout   | 0/1   | checkout-6b889d49cf-jt7bz | 0/1 nodes are available: 1 Insufficient cpu. no new claims to deallocate, preemption: 0/1 nodes are available: 1 Preemption is not helpful for scheduling.
(1 row)
```

## Find it and fix it in one statement

A query can also write. This annotates each Deployment in `shop` using under a fifth of
the memory it requests, so the finding sits on the object where `kubectl
describe` will show it. Each row is a real Kubernetes update.

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

[Changing the cluster](changing-the-cluster.md) covers the grant this needs,
and what happens when two writers race.

## Join the cluster to your own data

Axiom runs inside your application's Postgres, so the cluster joins to your own
tables with no export in between. Given which customer owns which namespace:

```sql
--8<-- "docs/snippets/examples/tenants-setup.sql"
```

Which customers are affected by a failing pod right now, and why:

```sql
--8<-- "docs/snippets/examples/tenants-failing.sql"
```

```
 customer  |    plan    |            pod            |      reason      
-----------+------------+---------------------------+------------------
 Acme Corp | enterprise | checkout-6b889d49cf-jt7bz | FailedScheduling
 Acme Corp | enterprise | checkout-worker           | BackOff
 Acme Corp | enterprise | report                    | Failed
(3 rows)
```

## More patterns

**Aggregate.** Where pods are failing, and how:

```sql
--8<-- "docs/snippets/examples/aggregate.sql"
```

```
 namespace |  phase  | count 
-----------+---------+-------
 shop      | Pending |     2
(1 row)
```

**Filter on a promoted column.** Deployments that never finished rolling out.
The replica counts are `bigint`, so they compare as numbers with no cast, and a
missing field is `NULL` rather than `0`:

```sql
--8<-- "docs/snippets/examples/filter-rollouts.sql"
```

```
 namespace |   name   | replicas | ready_replicas 
-----------+----------+----------+----------------
 shop      | checkout |        1 |               
(1 row)
```

**Filter on anything, through `raw`.** `kubectl` offers label selectors and a
few field selectors. Anything no column promotes is still in `raw`, such as
every container that sets no memory limit:

```sql
--8<-- "docs/snippets/examples/filter-raw.sql"
```

```
     namespace      |       deployment       |       container        
--------------------+------------------------+------------------------
 kube-system        | metrics-server         | metrics-server
 local-path-storage | local-path-provisioner | local-path-provisioner
 shop               | checkout               | checkout
 shop               | web                    | web
(4 rows)
```

**Ask several clusters at once.** Each cluster is its own server and its own
schema, so one question across them is a `UNION ALL`. This needs a second
cluster and gateway, so it is the shape rather than something to paste:

```sql
-- after CREATE SERVER stage ... and IMPORT FOREIGN SCHEMA k8s FROM SERVER stage INTO stage
SELECT 'prod' AS cluster, namespace, name, phase FROM k8s.core_pods   WHERE phase <> 'Running'
UNION ALL
SELECT 'stage',           namespace, name, phase FROM stage.core_pods WHERE phase <> 'Running';
```

## Bigger examples

- **[Capacity and risk review](capacity-review.md).** Per workload: what it
  uses, what it reserved, whether it is bounded at all, and what is failing.
  Usage, spec, ownership and events in one statement.
- **[Changing the cluster](changing-the-cluster.md).** `INSERT`, `UPDATE` and
  `DELETE` as real API calls, what Axiom refuses to do, and why.
- **[Manage Postgres with Postgres](manage-postgres.md).** One Postgres
  creating and scaling another through CloudNativePG, with no YAML.
- **[A silly Kubernetes operator in SQL](sql-operator.md).** The reconcile
  step is one `UPDATE … FROM` a table you own; `NOTIFY` wakes it.
- **[Your deployment, in chronological order](deploy-timeline.md).** A Helm
  release's Deployment, ReplicaSet, Pods, events and conditions, in order,
  from one query.
- **[Did your patch regress? pgbench across Postgres 16, 17 and 18](regression-lab.md).**
  CloudNativePG clusters created from a table, a `pgbench` Job against each,
  and each run's throughput beside the CPU its Postgres used.

!!! note "Custom resources and writes may need a grant"

    The shipped RBAC in `deploy/k8s/gateway-rbac.yaml` reads broadly, covering
    workloads, nodes, networking, storage, events and metrics, so most examples
    work as imported. It never reads Secrets, and it writes only ConfigMaps and
    the example CRD.

    A **custom resource** is readable if its operator ships an
    `aggregate-to-view` role. Otherwise, label a read-only ClusterRole for its
    API group `axiom.dhilipkumars.github.io/aggregate-to-gateway: "true"`
    ([Restricting access](../install/rbac.md#adding-a-custom-resource) shows
    one). To make a kind **writable**, add its verbs to the `axiom-gateway`
    ClusterRole. Neither needs a gateway restart; only revoking a grant does.
    Either way, re-import:

    ```sql
    -- foreign tables are catalog objects; they do not follow an RBAC change
    IMPORT FOREIGN SCHEMA k8s FROM SERVER prod INTO k8s;
    ```
