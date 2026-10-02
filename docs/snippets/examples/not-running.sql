SELECT DISTINCT ON (p.namespace, p.name)
       p.namespace, p.name AS pod, p.phase, e.reason, e.message
  FROM k8s.core_pods p
  JOIN k8s.core_events e ON e.namespace = p.namespace
                        AND e.involved_object->>'kind' = 'Pod'
                        AND e.involved_object->>'name' = p.name
 WHERE p.phase <> 'Running' AND e.type = 'Warning'
 ORDER BY p.namespace, p.name,
          coalesce(e.last_timestamp, e.event_time, e.creation_timestamp) DESC;
