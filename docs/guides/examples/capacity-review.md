# Capacity and risk review

Per workload: what it actually consumes, what it reserved, whether it is
bounded at all, and whether anything is failing. That takes usage, spec, the
ownership chain and events together, which is what no single `kubectl`
command can do, and Kubernetes forgets: events expire after about an hour and
usage is only ever *now*.

The output on this page is from a CI run against the `shop` namespace the
[Examples](index.md) use, on a one-node kind cluster.

## The tables involved

Usage comes from metrics-server as `metrics_k8s_io_pods` and
`metrics_k8s_io_nodes`. Events arrive as `core_events` and
`events_k8s_io_events`. Every table is named for its API group, which is why
core pods are `core_pods` beside `metrics_k8s_io_pods`.

## Quantities are strings until you convert them

Kubernetes reports measurements as strings with unit suffixes, `49903n` of CPU
and `14488Ki` of memory. Postgres cannot sum those, and `>` on them is a string
comparison that answers wrongly without an error. `axiom_quantity()` converts
exactly:

```sql
--8<-- "docs/snippets/examples/quantities.sql"
```

```
 100m  |   128Mi   | 1M = 1Mi 
-------+-----------+----------
 0.100 | 134217728 | f
(1 row)
```

`M` is a thousand thousand and `Mi` is 1024 × 1024, so they are not equal. It
returns `NULL` rather than raising for anything that is not a quantity, so one
odd field cannot fail a query that spans a cluster.

## The whole fleet in one statement

```sql
--8<-- "docs/snippets/examples/fleet-review.sql"
```

```
     namespace      |                    workload                     | pods | mem_used_mib | mem_requested_mib | pct_of_request | unbounded | warnings |  latest_warning  
--------------------+-------------------------------------------------+------+--------------+-------------------+----------------+-----------+----------+------------------
 kube-system        | kube-apiserver-axiom-e2e-control-plane          |    1 |        356.4 |                   |                | t         |        1 | NodeNotReady
 kube-system        | etcd-axiom-e2e-control-plane                    |    1 |         77.6 |             100.0 |             78 | t         |        1 | NodeNotReady
 kube-system        | kube-controller-manager-axiom-e2e-control-plane |    1 |         74.4 |                   |                | t         |        0 | 
 kube-system        | coredns                                         |    2 |         30.0 |             140.0 |             21 | f         |        2 | FailedScheduling
 kube-system        | kube-scheduler-axiom-e2e-control-plane          |    1 |         23.4 |                   |                | t         |        3 | NodeNotReady
 kube-system        | metrics-server                                  |    1 |         21.9 |             200.0 |             11 | t         |        0 | 
 axiom-system       | axiom-gateway                                   |    1 |         17.1 |              64.0 |             27 | f         |        0 | 
 kube-system        | kube-proxy-cdpt6                                |    1 |         16.0 |                   |                | t         |        0 | 
 kube-system        | kindnet-fm56z                                   |    1 |         13.4 |              50.0 |             27 | f         |        0 | 
 local-path-storage | local-path-provisioner                          |    1 |          8.8 |                   |                | t         |        1 | FailedScheduling
 shop               | web                                             |    2 |          0.4 |              32.0 |              1 | t         |        0 | 
 axiom-e2e          | web-0                                           |    1 |          0.2 |                   |                | t         |        0 | 
 shop               | catalog                                         |    1 |          0.2 |             256.0 |              0 | f         |        0 | 
 axiom-e2e          | db-0                                            |    1 |          0.2 |                   |                | t         |        0 | 
 axiom-e2e          | web-1                                           |    1 |          0.2 |                   |                | t         |        0 | 
 shop               | checkout                                        |    1 |              |                   |                | t         |        1 | FailedScheduling
 shop               | checkout-worker                                 |    1 |              |                   |                | t         |        7 | BackOff
 shop               | report                                          |    1 |              |                   |                | t         |        8 | Failed
(18 rows)
```

In `shop`, `catalog` reserves 256 MiB and uses 0.2, which the scheduler
cannot know. `checkout`, `checkout-worker` and `report` use nothing because
they never started, and why is on the same row. The largest consumer is the
API server, at 356 MiB with no limit set; on a cluster of your own, the top
rows are your workloads.

**Keep `usage` on a `LEFT JOIN`.** An inner join drops exactly the workloads
that have no metrics because they never started, which are the ones you most
want to see.

## Act on it

The review produces a list; a write turns it into an action.
[Find it and fix it in one statement](index.md#find-it-and-fix-it-in-one-statement)
annotates each Deployment in a namespace that uses under a fifth of the memory
it requests.

## Requirements

Usage tables need [metrics-server][ms] installed in the cluster; without it
`metrics.k8s.io` does not exist and the tables are simply absent. The gateway
also needs `list` on that group. The shipped RBAC grants it, so metrics-server
installed later is picked up on the next discovery refresh with no RBAC edit.
Custom and external metrics APIs, from an adapter such as KEDA or
prometheus-adapter, are granted the same way.

[ms]: https://github.com/kubernetes-sigs/metrics-server
