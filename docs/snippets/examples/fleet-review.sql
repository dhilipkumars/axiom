WITH usage AS (
  SELECT m.namespace, m.name AS pod,
         sum(axiom_quantity(c->'usage'->>'memory')) AS mem_used
    FROM k8s.metrics_k8s_io_pods m, jsonb_array_elements(m.containers) c
   GROUP BY 1, 2),
spec AS (
  SELECT p.namespace, p.name AS pod,
         p.metadata->'ownerReferences'->0->>'name' AS rs,
         sum(axiom_quantity(c->'resources'->'requests'->>'memory')) AS mem_req,
         bool_or(c->'resources'->'limits' IS NULL) AS no_limits
    FROM k8s.core_pods p, jsonb_array_elements(p.spec->'containers') c
   GROUP BY 1, 2, 3),
owner AS (
  SELECT r.namespace, r.name AS rs,
         coalesce(r.metadata->'ownerReferences'->0->>'name', r.name) AS workload
    FROM k8s.apps_replicasets r),
warn AS (
  SELECT e.namespace, e.involved_object->>'name' AS pod,
         sum(coalesce(e.count, 1)) AS warnings,
         max(coalesce(e.last_timestamp, e.event_time, e.creation_timestamp)) AS at,
         (array_agg(e.reason ORDER BY coalesce(e.last_timestamp, e.event_time,
                                               e.creation_timestamp) DESC))[1] AS why
    FROM k8s.core_events e
   WHERE e.type = 'Warning' AND e.involved_object->>'kind' = 'Pod'
   GROUP BY 1, 2)
SELECT s.namespace,
       coalesce(o.workload, s.pod) AS workload,
       count(*) AS pods,
       round(sum(u.mem_used) / 1024 / 1024, 1) AS mem_used_mib,
       round(sum(s.mem_req) / 1024 / 1024, 1) AS mem_requested_mib,
       CASE WHEN sum(s.mem_req) > 0
            THEN round(100 * sum(u.mem_used) / sum(s.mem_req)) END AS pct_of_request,
       bool_or(s.no_limits) AS unbounded,
       coalesce(sum(w.warnings), 0) AS warnings,
       (array_agg(w.why ORDER BY w.at DESC NULLS LAST))[1] AS latest_warning
  FROM spec s
  LEFT JOIN usage u ON u.namespace = s.namespace AND u.pod = s.pod
  LEFT JOIN owner o ON o.namespace = s.namespace AND o.rs = s.rs
  LEFT JOIN warn  w ON w.namespace = s.namespace AND w.pod = s.pod
 GROUP BY s.namespace, coalesce(o.workload, s.pod)
 ORDER BY mem_used_mib DESC NULLS LAST;
