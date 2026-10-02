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
<!-- output:quantities -->
```

`M` is a thousand thousand and `Mi` is 1024 × 1024, so they are not equal. It
returns `NULL` rather than raising for anything that is not a quantity, so one
odd field cannot fail a query that spans a cluster.

## The whole fleet in one statement

```sql
--8<-- "docs/snippets/examples/fleet-review.sql"
```

```
<!-- output:fleet-review -->
```

<!-- reading:fleet-review -->

**Keep `usage` on a `LEFT JOIN`.** An inner join drops exactly the workloads
that have no metrics because they never started, which are the ones you most
want to see.

## Act on it

The review produces a list; a write turns it into an action.
[Find it and fix it in one statement](index.md#find-it-and-fix-it-in-one-statement)
annotates every Deployment that uses under a fifth of the memory it requests.

## Requirements

Usage tables need [metrics-server][ms] installed in the cluster; without it
`metrics.k8s.io` does not exist and the tables are simply absent. The gateway
also needs `list` on that group. The shipped RBAC grants it, so metrics-server
installed later is picked up on the next discovery refresh with no RBAC edit.
Custom and external metrics APIs, from an adapter such as KEDA or
prometheus-adapter, are granted the same way.

[ms]: https://github.com/kubernetes-sigs/metrics-server
