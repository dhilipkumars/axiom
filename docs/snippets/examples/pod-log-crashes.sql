SELECT p.name AS pod, l.ts, l.line
  FROM k8s.core_pods p,
       LATERAL axiom_pod_log('prod', p.namespace, p.name, tail_lines => 20,
         previous => coalesce(p.status->'containerStatuses'->0->'state' ? 'running', false)) l
 WHERE p.namespace = 'shop'
   AND (p.status->'containerStatuses'->0->>'restartCount')::int > 0
 ORDER BY p.name, l.line_no;
