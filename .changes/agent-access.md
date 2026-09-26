---
kind: security
---

**`axiom_watch_status()` is no longer callable by every role.** It lists every
watched server, resource and namespace, with object counts and resource
versions, whatever the caller's table grants. So a role granted only a narrow
view could learn what the cluster is being watched for. Only superusers can
call it now. Grant it to the roles that monitor Axiom:

```sql
GRANT EXECUTE ON FUNCTION axiom_watch_status() TO monitoring;
```

A new guide, **Giving an AI agent access**, sets out a role that gives an
agent redacted, tenant-scoped access to the cluster. The agent gets no shell
and no Kubernetes credential. The guide also covers the grants never to give
an agent and the settings that look like controls but are not. An end-to-end
test runs its recipe against a real cluster.
