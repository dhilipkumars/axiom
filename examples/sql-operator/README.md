# An operator in SQL

A Kubernetes operator watches for objects that drift from a desired state and
writes them back. With Axiom the reconcile step is one statement:

```sql
UPDATE sqlop.configmaps c
   SET labels = coalesce(c.labels, '{}'::jsonb) || jsonb_build_object('team', o.team)
  FROM sqlop.owners o
 WHERE c.namespace = o.namespace
   AND c.labels->>'team' IS DISTINCT FROM o.team;
```

The desired state is `sqlop.owners`, an ordinary table saying which team owns
each namespace. The operator keeps a `team` label on every ConfigMap in those
namespaces equal to it. Add a ConfigMap and it is labelled within seconds;
remove the label by hand and it comes back; change a row in `owners` and every
ConfigMap in that namespace is relabelled. The desired state lives in your
database, next to the rest of your data, and can come from anything SQL can
express.

## Running it

```sh
psql -v server=<your axiom server> -f setup.sql
psql -c "INSERT INTO sqlop.owners VALUES ('default', 'payments')"
PGDATABASE=<your database> ./operator.sh
```

`operator.sh` needs only `bash` and `psql`, and reads its connection from the
usual `PG*` variables. It reconciles once at startup, again whenever a
ConfigMap changes, and on a timer (`SWEEP_SECONDS`, 30 by default). If
`axiom.notify_database` is not the database you run it in, set
`NOTIFY_DATABASE` to it: notifications are sent there.

The gateway needs `update` on ConfigMaps, which the shipped RBAC grants.

## Why it looks like this

**It is level-triggered.** `reconcile.sql` compares the whole desired state
with the whole actual state every time, and writes only what differs. Run it
twice and the second run changes nothing; run it after something was missed
and it catches up. That is how Kubernetes' own controllers work, and it is
what makes the rest of this safe.

**The notifications are only there to make it fast.** `setup.sql` creates a
second, watched table, `sqlop.configmaps_watched`. Watching it makes the
extension send `NOTIFY axiom_events` whenever a ConfigMap changes, and
`operator.sh` reconciles as soon as one arrives. But a notification is a hint:
none is queued while nothing is listening, and the extension sends none for
the objects a watch finds when it starts. The periodic sweep is what makes the
operator correct; the notifications only make it quick.

**It writes through a table that is not watched.** A watched table answers
from the extension's cache, which can be a moment behind the cluster. An
`UPDATE` carries the `resourceVersion` it read, and a stale one only produces
a conflict, so `sqlop.configmaps` reads from the API server every time.

**A pass is not atomic across objects.** Each row is its own write. If another
writer changes the fifth ConfigMap in between, that write fails with
SQLSTATE 40001, the first four stay written, and `operator.sh` logs the
failure and carries on; the next pass finishes the job.

## Limits

- **It acts as the gateway.** Every write uses the gateway's ServiceAccount,
  whichever SQL role runs the operator (#71), and any role that can use the
  server can do the same.
- **`LISTEN axiom_events` is not scoped.** Any role can listen and learn which
  objects changed; see the agent-access guide.
- **Each pass lists every ConfigMap in the cluster.** The join on `namespace`
  is not a constant, so it is not pushed to the gateway. For a few namespaces,
  a `WHERE c.namespace IN (...)` with literal names narrows it.
- **Labels are the easy case.** They sit at a fixed path, `metadata.labels`. A
  camelCase top-level field the object does not have yet is written under its
  snake_case name until #97 is fixed.
