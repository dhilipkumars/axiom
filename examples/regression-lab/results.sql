-- Every run in the matrix, with its result once it has one.
--
--   psql -v namespace=regression-lab -f results.sql
--
-- A finished container's termination message is in the Pod's status. It is
-- JSON when pgbench succeeded and the tail of the container's output when it
-- did not, so it is parsed only when it is JSON and shown as the error when
-- the run failed. Resource use comes from lab.usage, which sample.sql fills
-- while runs are going; without metrics-server those columns stay empty.
\set ON_ERROR_STOP on

WITH finished AS (
  SELECT p.labels->>'axiom-lab/run' AS run, cs->'state'->'terminated' AS t
    FROM lab.pods p, jsonb_array_elements(p.status->'containerStatuses') cs
   WHERE p.namespace = :'namespace'
     AND p.labels ? 'axiom-lab/run'
     AND cs->>'name' = 'pgbench'
),
result AS (
  SELECT run,
         (t->>'exitCode')::int AS exit_code,
         CASE WHEN t->>'message' IS JSON OBJECT THEN (t->>'message')::jsonb END AS r,
         t->>'message' AS message
    FROM finished
),
usage AS (
  SELECT run, max(cpu_cores) AS peak_cpu_cores, max(memory) AS peak_memory
    FROM lab.usage GROUP BY run
)
SELECT m.run, m.settings,
       CASE WHEN r.exit_code IS NULL THEN 'running'
            WHEN r.exit_code = 0 THEN 'done'
            ELSE 'failed' END AS state,
       (r.r->>'tps')::numeric AS tps,
       (r.r->>'latency_ms')::numeric AS latency_ms,
       r.r->>'server_version' AS server_version,
       u.peak_cpu_cores,
       pg_size_pretty(u.peak_memory) AS peak_memory,
       -- One line, so the table stays readable: the message is the tail of
       -- the container's output, newlines and all.
       CASE WHEN r.exit_code <> 0
            THEN left(regexp_replace(r.message, '\s+', ' ', 'g'), 400) END AS error
  FROM lab.matrix m
  LEFT JOIN result r USING (run)
  LEFT JOIN usage u USING (run)
 ORDER BY tps DESC NULLS LAST, m.run;
