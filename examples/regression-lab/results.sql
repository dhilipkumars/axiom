-- Every run in the matrix, with where it has got to and its result once it
-- has one.
--
--   psql -v namespace=regression-lab -f results.sql
--
-- state is one of:
--   not started  no Pod for the run yet: not launched, or not yet created
--   pending      a Pod, not yet scheduled or started
--   waiting      scheduled, but the container cannot start; `error` says why
--                (ImagePullBackOff, CrashLoopBackOff, ...)
--   running      the benchmark is going
--   done         finished, with a result
--   failed       finished without one; `error` is the tail of its output
--
-- A finished container's termination message is in the Pod's status. It is
-- JSON when pgbench succeeded and the tail of the container's output when it
-- did not. Resource use comes from lab.usage, which sample.sql fills while
-- runs are going; without metrics-server those columns stay empty.
\set ON_ERROR_STOP on

WITH latest_pod AS (
  -- One Pod per run: the newest, if a run was relaunched or its Pod evicted.
  SELECT DISTINCT ON (p.labels->>'axiom-lab/run')
         p.labels->>'axiom-lab/run' AS run,
         p.status->>'phase' AS phase,
         -- Why a Pod failed before any container ran, e.g. Evicted.
         concat_ws(': ', p.status->>'reason', p.status->>'message') AS pod_reason,
         (SELECT cs FROM jsonb_array_elements(p.status->'containerStatuses') cs
           WHERE cs->>'name' = 'pgbench') AS cs
    FROM lab.pods p
   WHERE p.namespace = :'namespace'
     AND p.labels ? 'axiom-lab/run'
   ORDER BY p.labels->>'axiom-lab/run', p.creation_timestamp DESC
),
result AS (
  SELECT run, phase, pod_reason,
         -- Waiting to be created is normal on the way to starting; any other
         -- reason (ImagePullBackOff, CrashLoopBackOff, ...) is a problem.
         CASE WHEN cs->'state'->'waiting'->>'reason' NOT IN ('ContainerCreating', 'PodInitializing')
              THEN cs->'state'->'waiting' END AS waiting,
         cs->'state'->'terminated' AS t,
         (cs->'state'->'terminated'->>'exitCode')::int AS exit_code,
         CASE WHEN cs->'state'->'terminated'->>'message' IS JSON OBJECT
              THEN (cs->'state'->'terminated'->>'message')::jsonb END AS r,
         -- Finished with a result. A CASE, because it is the only place
         -- Postgres guarantees evaluation order: in an AND the cast could run
         -- before the IS JSON test and fail on a failed run's log text. Never
         -- NULL, because NOT NULL would hide the failure it should report.
         CASE WHEN (cs->'state'->'terminated'->>'exitCode')::int IS DISTINCT FROM 0 THEN false
              WHEN (cs->'state'->'terminated'->>'message') IS JSON OBJECT
              THEN coalesce((cs->'state'->'terminated'->>'message')::jsonb ? 'tps', false)
              ELSE false END AS ok
    FROM latest_pod
),
usage AS (
  SELECT run, max(cpu_cores) AS peak_cpu_cores, max(memory) AS peak_memory
    FROM lab.usage
   WHERE namespace = :'namespace'
   GROUP BY run
)
SELECT m.run, m.settings,
       CASE WHEN r.run IS NULL THEN 'not started'
            WHEN r.ok THEN 'done'
            WHEN r.t IS NOT NULL OR r.phase = 'Failed' THEN 'failed'
            WHEN r.waiting IS NOT NULL THEN 'waiting'
            WHEN r.phase = 'Running' THEN 'running'
            ELSE 'pending' END AS state,
       (r.r->>'tps')::numeric AS tps,
       (r.r->>'latency_ms')::numeric AS latency_ms,
       r.r->>'server_version' AS server_version,
       u.peak_cpu_cores,
       pg_size_pretty(u.peak_memory) AS peak_memory,
       -- One line, so the table stays readable: a termination message is the
       -- tail of the container's output, newlines and all.
       CASE WHEN r.t IS NOT NULL AND NOT r.ok
            THEN left(regexp_replace(coalesce(nullif(r.t->>'message', ''), 'exit code ' || r.exit_code),
                                     '\s+', ' ', 'g'), 400)
            WHEN r.phase = 'Failed' THEN nullif(r.pod_reason, '')
            WHEN r.waiting IS NOT NULL
            THEN concat_ws(': ', r.waiting->>'reason', r.waiting->>'message') END AS error
  FROM lab.matrix m
  LEFT JOIN result r USING (run)
  LEFT JOIN usage u USING (run)
 ORDER BY tps DESC NULLS LAST, m.run;
