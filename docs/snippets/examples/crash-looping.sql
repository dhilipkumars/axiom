SELECT namespace, name AS pod, phase, c->>'name' AS container,
       (c->>'restartCount')::int AS restarts,
       coalesce(c->'state'->'waiting'->>'reason',
                c->'lastState'->'terminated'->>'reason') AS why
  FROM k8s.core_pods,
       jsonb_array_elements(status->'containerStatuses') c
 WHERE (c->>'restartCount')::int > 0
 ORDER BY restarts DESC;
