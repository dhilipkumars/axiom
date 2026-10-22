---
description: "Read Kubernetes pod logs from SQL with Axiom: a crash's backtrace next to the pod that crashed, bounded in lines and bytes, and governed per Postgres role."
---

# Reading pod logs

SQL can find every failing pod and the warning that explains it. With
`axiom_pod_log`, it can also show what the container printed before it died,
in the same query.

## Turn it on

Logs are off by default. They often contain secrets, and the shipped RBAC
already lets the gateway read them (Kubernetes' `view` role includes
`pods/log`), so RBAC can't be the switch. Start the gateway with
`-serve-pod-logs`, which also needs `pods` in `-serve`:

```sh
kubectl -n axiom-system patch deploy/axiom-gateway --type=json -p \
  '[{"op":"add","path":"/spec/template/spec/containers/0/args/-","value":"-serve-pod-logs"}]'
```

Until then, every call fails with an error naming `--serve-pod-logs`.

## One pod's log

```sql
--8<-- "docs/snippets/examples/pod-log.sql"
```

```
 container_name | line_no |            line            
----------------+---------+----------------------------
 app            |       1 | listening on :8080
 app            |       2 | GET /cart 200 12ms
 app            |       3 | GET /search?q=caf� 200 9ms
(3 rows)
```

`frontend` has two containers, `envoy` and `app`. With no `container`
argument, the one read is the one `kubectl logs` would pick: the container
named by the `kubectl.kubernetes.io/default-container` annotation, otherwise
the first. `container_name` says which it was. The last request came from a
client that sent Latin-1, and the byte that isn't valid UTF-8 comes back as `�`
rather than failing the query.

## Why did it crash?

`previous => true` reads the instance before the container's last restart, as
`kubectl logs --previous` does. Kubernetes keeps one exited instance per
container. So once a container is running again, its crash is in the previous
instance. While it's still down, the crash is the current instance, and the
one before it is usually gone.

So ask for `previous` only when the container is running again. `LATERAL`
then reads the latest crash of every pod that has restarted, in one statement:

```sql
--8<-- "docs/snippets/examples/pod-log-crashes.sql"
```

```
   pod    |              ts               |                             line                             
----------+-------------------------------+--------------------------------------------------------------
 payments | 2026-10-20 20:58:33.054297+00 | payments v1.4 starting
 payments | 2026-10-20 20:58:33.054327+00 | panic: dial tcp 10.96.0.15:5432: connect: connection refused
(2 rows)
```

`payments` panicked once and is running again, so its panic is in the previous
instance. A pod with no such log adds no rows: one that has never restarted,
a container that hasn't started, or a previous instance Kubernetes has already
removed. None of these is an error, so one such pod doesn't fail a query over
many.

## The arguments

```sql
axiom_pod_log(server text, namespace text, pod text,
              container text      DEFAULT NULL,
              tail_lines integer  DEFAULT 100,
              since timestamptz   DEFAULT NULL,
              previous boolean    DEFAULT false,
              max_bytes integer   DEFAULT 1048576)
  RETURNS TABLE (container_name text, line_no integer, ts timestamptz,
                 line text, truncated boolean)
```

- **`tail_lines`** is how many of the newest lines to read, from 1 to 100,000.
- **`since`** keeps only lines at or after a time, to the second:
  `since => now() - interval '1 hour'`, or `since => p.creation_timestamp`.
- **`max_bytes`** caps the text returned, up to 8 MiB. When the lines selected
  hold more, the **newest** bytes are kept, because the end of a log is where a
  crash is, and every row has `truncated = true`. A single line longer than
  `max_bytes` comes back cut, also marked.
- **`ts`** is when the container runtime recorded the line, or NULL if it gave
  no timestamp.

Logs are never cached. Every call is one live read through the gateway, and
it's subject to the server's `rpc_timeout_secs`. A `LATERAL` join over a
thousand pods makes a thousand calls, so narrow the pods first.

## Who can call it

`axiom_pod_log` isn't executable by `PUBLIC`, and it also needs `USAGE` on the
server. Query roles don't hold `USAGE` (see
[Giving an AI agent access](../agent-access.md)), so a role gets logs through a
wrapper that fixes the server and the namespace. The wrapper is the
[recipe in the agent guide](../agent-access.md#letting-an-agent-read-logs).
