SELECT namespace, name AS pod, c->>'name' AS container,
       (c->>'restartCount')::int AS restarts
  FROM k8s.core_pods,
       jsonb_array_elements(status->'containerStatuses') c
 WHERE c->'state'->'waiting'->>'reason' = 'CrashLoopBackOff';
