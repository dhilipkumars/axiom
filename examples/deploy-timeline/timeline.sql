-- Everything that happened to one release, on one timeline.
--
--   psql -v namespace=default -v release=demo -f timeline.sql
--
-- Needs these tables from IMPORT FOREIGN SCHEMA (see README.md):
--   k8s.apps_deployments, k8s.apps_replicasets, k8s.core_pods,
--   k8s.core_services, k8s.core_events
--
-- Objects are found by the release's app.kubernetes.io/instance label, then
-- followed through ownerReferences -- Deployment to ReplicaSet to Pod -- by
-- uid, never by name: a name is reused by the next object of the same kind,
-- a uid is not. Events are matched to those objects the same way.
\set ON_ERROR_STOP on

WITH deployments AS (
  SELECT 'Deployment' AS kind, name, uid, creation_timestamp
    FROM k8s.apps_deployments
   WHERE namespace = :'namespace'
     AND labels->>'app.kubernetes.io/instance' = :'release'
),
replicasets AS (
  SELECT 'ReplicaSet' AS kind, r.name, r.uid, r.creation_timestamp
    FROM k8s.apps_replicasets r
    JOIN deployments d
      ON r.metadata->'ownerReferences' @> jsonb_build_array(jsonb_build_object('uid', d.uid))
   WHERE r.namespace = :'namespace'
),
pods AS (
  SELECT 'Pod' AS kind, p.name, p.uid, p.creation_timestamp, p.status
    FROM k8s.core_pods p
    JOIN replicasets r
      ON p.metadata->'ownerReferences' @> jsonb_build_array(jsonb_build_object('uid', r.uid))
   WHERE p.namespace = :'namespace'
),
services AS (
  SELECT 'Service' AS kind, name, uid, creation_timestamp
    FROM k8s.core_services
   WHERE namespace = :'namespace'
     AND labels->>'app.kubernetes.io/instance' = :'release'
),
objects AS (
  SELECT kind, name, uid, creation_timestamp FROM deployments
  UNION ALL SELECT kind, name, uid, creation_timestamp FROM replicasets
  UNION ALL SELECT kind, name, uid, creation_timestamp FROM pods
  UNION ALL SELECT kind, name, uid, creation_timestamp FROM services
),
steps AS (
  -- Each object's creation.
  SELECT creation_timestamp AS at, kind, name, 'created' AS step, NULL::text AS detail
    FROM objects
  UNION ALL
  -- Each Pod condition that has come true: PodScheduled, Initialized,
  -- ContainersReady, Ready. These outlive the events, which expire.
  SELECT (c->>'lastTransitionTime')::timestamptz, 'Pod', p.name, c->>'type', NULL
    FROM pods p, jsonb_array_elements(p.status->'conditions') c
   WHERE c->>'status' = 'True'
  UNION ALL
  -- Every event about one of the objects. The scheduler sets only
  -- event_time and the kubelet only first_timestamp, hence the coalesce.
  -- A Pulled event's message says how long the pull took; show just that.
  SELECT coalesce(e.event_time, e.first_timestamp, e.creation_timestamp), o.kind, o.name, e.reason,
         coalesce('pull took ' || substring(e.message from 'Successfully pulled image "[^"]*" in ([^ ]+)'),
                  e.message)
    FROM k8s.core_events e
    JOIN objects o ON e.involved_object->>'uid' = o.uid
   WHERE e.namespace = :'namespace'
)
SELECT at, kind, name, step, detail
  FROM steps
 -- Kubernetes timestamps are whole seconds, so several steps share one. Within
 -- a second, order them the way they have to happen.
 ORDER BY at,
          CASE step
            WHEN 'created' THEN CASE kind WHEN 'Deployment' THEN 1 WHEN 'Service' THEN 2
                                          WHEN 'ReplicaSet' THEN 3 ELSE 4 END
            WHEN 'ScalingReplicaSet' THEN 2
            WHEN 'SuccessfulCreate' THEN 4
            WHEN 'Scheduled' THEN 5 WHEN 'PodScheduled' THEN 5
            WHEN 'Pulling' THEN 6 WHEN 'Pulled' THEN 7
            WHEN 'Created' THEN 8 WHEN 'Started' THEN 9
            WHEN 'PodReadyToStartContainers' THEN 10 WHEN 'Initialized' THEN 10
            WHEN 'ContainersReady' THEN 11 WHEN 'Ready' THEN 12
            ELSE 13
          END,
          kind, name;
