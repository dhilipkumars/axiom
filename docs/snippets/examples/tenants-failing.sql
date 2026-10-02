SELECT DISTINCT ON (p.namespace, p.name)
       t.customer, t.plan, p.name AS pod, e.reason
  FROM tenants t
  JOIN k8s.core_pods p ON p.namespace = t.namespace
  JOIN k8s.core_events e ON e.namespace = p.namespace
                        AND e.involved_object->>'kind' = 'Pod'
                        AND e.involved_object->>'name' = p.name
 WHERE e.type = 'Warning'
 ORDER BY p.namespace, p.name,
          coalesce(e.last_timestamp, e.event_time, e.creation_timestamp) DESC;
