---
kind: added
---

**Read pod logs from SQL with `axiom_pod_log`.** It returns the newest lines of
one container's log as rows, so a crash's output can sit next to the pod that
crashed: `LATERAL axiom_pod_log(server, p.namespace, p.name, previous => true)`
reads every failed pod's previous instance in one query. Results are bounded
by `tail_lines` and `max_bytes`. When the log holds more than `max_bytes`, the
newest bytes are kept and `truncated` is set. With no container named, it reads
the one `kubectl logs` would.

Off by default: start the gateway with `-serve-pod-logs`, which also needs
pods in `-serve`. Logs often contain secrets, and the shipped RBAC already
grants `pods/log` through Kubernetes' `view` role, so the flag rather than
RBAC is the switch. The function isn't executable by `PUBLIC` and needs
`USAGE` on the server, so roles reach it through a `SECURITY DEFINER` wrapper
that fixes the namespace, as the agent-access guide shows.
