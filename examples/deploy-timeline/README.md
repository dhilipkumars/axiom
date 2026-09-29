# A deploy, on one timeline

`helm install` returns quickly; what happens next is spread across a
Deployment, a ReplicaSet, one or more Pods, a Service and dozens of events,
and `kubectl` shows each of them separately. `timeline.sql` puts them in one
list:

```
2026-09-29 10:01:02+00 | Deployment | demo            | created           |
2026-09-29 10:01:02+00 | Deployment | demo            | ScalingReplicaSet | Scaled up replica set demo-6d4b7 from 0 to 1
2026-09-29 10:01:02+00 | ReplicaSet | demo-6d4b7      | created           |
2026-09-29 10:01:02+00 | ReplicaSet | demo-6d4b7      | SuccessfulCreate  | Created pod: demo-6d4b7-x2k9q
2026-09-29 10:01:02+00 | Pod        | demo-6d4b7-x2k9q| created           |
2026-09-29 10:01:02+00 | Pod        | demo-6d4b7-x2k9q| Scheduled         | Successfully assigned ...
2026-09-29 10:01:03+00 | Pod        | demo-6d4b7-x2k9q| Pulled            | pull took 1.832s
2026-09-29 10:01:04+00 | Pod        | demo-6d4b7-x2k9q| Started           | Started container app
2026-09-29 10:01:05+00 | Pod        | demo-6d4b7-x2k9q| Ready             |
```

It is one query over five tables: find the release's Deployment and Service
by label, follow `ownerReferences` from the Deployment to its ReplicaSets and
their Pods, then add each object's creation, every event about it, and each
Pod condition as it came true.

## Running it

```sh
# The tables it reads.
psql -c "IMPORT FOREIGN SCHEMA k8s
           LIMIT TO (apps_deployments, apps_replicasets, core_pods, core_services, core_events)
           FROM SERVER <your axiom server> INTO k8s"

helm install demo ./chart -n default
psql -v namespace=default -v release=demo -f timeline.sql
```

Any release works, not only the demo chart: the query keys on the
`app.kubernetes.io/instance` label, which Helm charts set by convention.

## What it can and cannot see

- **Everything is joined by uid, not by name.** The next Pod of a Deployment
  reuses nothing but the name pattern; a uid is never reused, so a timeline
  never mixes two rollouts.
- **Secrets are not on it.** The gateway's shipped RBAC excludes Secrets by
  design, so Helm's own release record, which it keeps in a Secret, is not
  visible. The release is identified by its labels instead.
- **Events expire.** The API server keeps them for an hour by default, so the
  timeline of an older release shows creations and Pod conditions but not the
  events in between.
- **Load-balancer steps need a cloud controller.** The demo chart asks for a
  `LoadBalancer` Service; where a cloud controller provisions one, its
  `EnsuringLoadBalancer` and `EnsuredLoadBalancer` events join the timeline.
  kind has none, and the Service stays pending.
- **Timestamps are whole seconds.** Several steps often share a second; within
  one, the query orders them the way they must have happened.
