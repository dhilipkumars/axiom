-- Each finished benchmark, read straight from its Pod: pgbench's result is the
-- container's termination message, which Kubernetes keeps in the Pod's status.
--
--   psql -f summary.sql
--
-- results.sql is the full picture: every run's state, and the CPU and memory
-- its Postgres used while it ran.
SELECT p.labels->>'axiom-lab/run'                        AS run,
       split_part(m.r->>'server_version', ' ', 1)        AS postgres,
       round((m.r->>'tps')::numeric)                     AS tps,
       (m.r->>'latency_ms')::numeric                     AS latency_ms
  FROM lab.pods p,
       jsonb_array_elements(p.status->'containerStatuses') cs,
       LATERAL (SELECT (cs->'state'->'terminated'->>'message')::jsonb AS r) m
 WHERE p.namespace = 'regression-lab'
   AND cs->>'name' = 'pgbench'
   AND cs->'state'->'terminated'->>'reason' = 'Completed'
 ORDER BY run;
