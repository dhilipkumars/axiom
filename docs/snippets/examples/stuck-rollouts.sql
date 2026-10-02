SELECT DISTINCT ON (d.namespace, d.name, p.name)
       d.namespace, d.name AS deployment,
       coalesce(d.ready_replicas, 0) || '/' || d.replicas AS ready,
       p.name AS pod, e.message AS why
  FROM k8s.apps_deployments d
  JOIN k8s.apps_replicasets r ON r.namespace = d.namespace
                             AND r.metadata->'ownerReferences'->0->>'name' = d.name
  JOIN k8s.core_pods p ON p.namespace = r.namespace
                      AND p.metadata->'ownerReferences'->0->>'name' = r.name
  LEFT JOIN k8s.core_events e ON e.namespace = p.namespace
                             AND e.involved_object->>'kind' = 'Pod'
                             AND e.involved_object->>'name' = p.name
                             AND e.type = 'Warning'
 WHERE coalesce(d.ready_replicas, 0) < d.replicas
   AND p.phase <> 'Running'
 ORDER BY d.namespace, d.name, p.name,
          coalesce(e.last_timestamp, e.event_time, e.creation_timestamp) DESC NULLS LAST;
