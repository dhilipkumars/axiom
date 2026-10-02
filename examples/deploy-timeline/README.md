# Your deployment, in chronological order

`helm install` returns quickly; what happens next is spread across a
Deployment, a ReplicaSet, one or more Pods, a Service and dozens of events,
and `kubectl` shows each of them separately. `timeline.sql` puts them in one
list:

```
           at           |    kind    |         name         |           step            |          detail
------------------------+------------+----------------------+---------------------------+------------------------------------------------------------
 2026-09-30 13:21:20+00 | Deployment | demo                 | created                   |
 2026-09-30 13:21:20+00 | Deployment | demo                 | ScalingReplicaSet         | Scaled up replica set demo-9b47f4794 from 0 to 1
 2026-09-30 13:21:20+00 | Service    | demo                 | created                   |
 2026-09-30 13:21:20+00 | ReplicaSet | demo-9b47f4794       | created                   |
 2026-09-30 13:21:20+00 | Pod        | demo-9b47f4794-lzkc8 | created                   |
 2026-09-30 13:21:20+00 | ReplicaSet | demo-9b47f4794       | SuccessfulCreate          | Created pod: demo-9b47f4794-lzkc8
 2026-09-30 13:21:20+00 | Pod        | demo-9b47f4794-lzkc8 | Scheduled                 | Successfully assigned axiom-timeline/demo-9b47f4794-lzkc8 to axiom-e2e-control-plane
 2026-09-30 13:21:20+00 | Pod        | demo-9b47f4794-lzkc8 | PodScheduled              |
 2026-09-30 13:21:20+00 | Pod        | demo-9b47f4794-lzkc8 | Initialized               |
 2026-09-30 13:21:20+00 | Pod        | demo-9b47f4794-lzkc8 | Pulled                    | Container image "registry.k8s.io/pause:3.10" already present on machine ...
 2026-09-30 13:21:20+00 | Pod        | demo-9b47f4794-lzkc8 | Created                   | Container created
 2026-09-30 13:21:20+00 | Pod        | demo-9b47f4794-lzkc8 | Started                   | Container started
 2026-09-30 13:21:21+00 | Pod        | demo-9b47f4794-lzkc8 | PodReadyToStartContainers |
 2026-09-30 13:21:21+00 | Pod        | demo-9b47f4794-lzkc8 | ContainersReady           |
 2026-09-30 13:21:21+00 | Pod        | demo-9b47f4794-lzkc8 | Ready                     |
(15 rows)
```

That is the demo chart on kind, from the e2e suite's run: the whole rollout
in about a second. The image was already on the node, so `Pulled` says so;
with an image that has to be fetched, the row says how long the pull took
instead. Where steps share a second, the query orders them the way they have
to happen; `PodReadyToStartContainers` lands a second later only because that
is when the kubelet recorded it.

It is one query over five tables: find the release's Deployment and Service
by label, follow `ownerReferences` from the Deployment to its ReplicaSets and
their Pods, then add each object's creation, every event about it, and each
Pod condition as it came true.

## Running it

```sh
# The tables it reads, in a schema of their own.
psql -c "CREATE SCHEMA IF NOT EXISTS k8s"
psql -c "IMPORT FOREIGN SCHEMA k8s
           LIMIT TO (apps_deployments, apps_replicasets, core_pods, core_services, core_events)
           FROM SERVER <your axiom server> INTO k8s"

helm install demo ./chart -n default
psql -v namespace=default -v release=demo -f timeline.sql
```

Any release works, not only the demo chart: the query keys on the
`app.kubernetes.io/instance` label, which Helm charts set by convention.

## What it can and cannot see

- **Everything is joined by uid, not by name.** A Deployment deleted and
  created again under the same name has a new uid, so its predecessor's
  objects and events never leak into the timeline.
- **Every rollout of the release is on it.** An upgrade or a
  `rollout restart` keeps the Deployment's uid and adds a ReplicaSet, and the
  old ReplicaSets stay (ten by default, `revisionHistoryLimit`). Each rollout's
  rows carry its own ReplicaSet name; filter on it to see one rollout alone.
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
