-- Every run: where it has got to, its result, and what the Postgres under test
-- used while it ran.
--
--   psql -v namespace=regression-lab -f results.sql
--
-- state is one of:
--   not started  no Pod for the run yet: not launched, or not yet created
--   pending      a Pod, not yet started
--   waiting      the container cannot start; `error` says why
--   running      the benchmark is going
--   done         finished, with a result
--   failed       finished without one; `error` is the tail of its output
--
-- Resource use is taken from lab.usage (sample.sql) for the run's cluster,
-- counting only the samples whose whole window falls inside the timed run --
-- not the Job's start and end, which also cover waiting for the cluster and
-- building the pgbench tables. `samples` says how many that was.
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
   -- Timestamps are whole seconds; the name breaks a tie deterministically.
   ORDER BY p.labels->>'axiom-lab/run', p.creation_timestamp DESC, p.name DESC
),
result AS (
  SELECT run, phase, pod_reason,
         -- Waiting to be created is normal on the way to starting; any other
         -- reason (ImagePullBackOff, CreateContainerConfigError, ...) is not.
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
used AS (
  SELECT r.run, u.role,
         count(*) AS samples,
         avg(u.cpu_cores) AS cpu_avg,
         max(u.cpu_cores) AS cpu_peak,
         max(u.memory) AS memory_peak
    FROM result r
    JOIN lab.runs x USING (run)
    JOIN lab.usage u
      ON u.namespace = :'namespace'
     AND u.cluster = x.cluster
     AND u.sampled_at - make_interval(secs => coalesce(u.window_s, 15))
           >= (r.r->>'started_at')::timestamptz
     AND u.sampled_at <= (r.r->>'finished_at')::timestamptz
   WHERE r.ok
   GROUP BY r.run, u.role
)
SELECT x.run, x.cluster, r.r->>'server_version' AS server_version, c.cpu AS cpu_limit, x.clients,
       CASE WHEN r.run IS NULL THEN 'not started'
            WHEN r.ok THEN 'done'
            WHEN r.t IS NOT NULL OR r.phase = 'Failed' THEN 'failed'
            WHEN r.waiting IS NOT NULL THEN 'waiting'
            WHEN r.phase = 'Running' THEN 'running'
            ELSE 'pending' END AS state,
       round((r.r->>'tps')::numeric) AS tps,
       (r.r->>'latency_ms')::numeric AS latency_ms,
       round(pg.cpu_avg, 2) AS pg_cpu_avg,
       -- Work per core actually used: comparable across runs even when the
       -- server did not use all it was given.
       round((r.r->>'tps')::numeric / nullif(pg.cpu_avg, 0)) AS tps_per_core,
       round(pg.cpu_peak, 2) AS pg_cpu_peak,
       pg_size_pretty(round(pg.memory_peak)) AS pg_memory_peak,
       pg.samples,
       round(bench.cpu_avg, 2) AS pgbench_cpu_avg,
       -- One line, so the table stays readable: a termination message is the
       -- tail of the container's output, newlines and all.
       CASE WHEN r.t IS NOT NULL AND NOT r.ok
            THEN left(regexp_replace(coalesce(nullif(r.t->>'message', ''), 'exit code ' || r.exit_code),
                                     '\s+', ' ', 'g'), 300)
            WHEN r.phase = 'Failed' THEN nullif(r.pod_reason, '')
            WHEN r.waiting IS NOT NULL
            THEN concat_ws(': ', r.waiting->>'reason', r.waiting->>'message') END AS error
  FROM lab.runs x
  LEFT JOIN lab.clusters c ON c.name = x.cluster
  LEFT JOIN result r USING (run)
  LEFT JOIN used pg ON pg.run = x.run AND pg.role = 'postgres'
  LEFT JOIN used bench ON bench.run = x.run AND bench.role = 'pgbench'
 ORDER BY x.run;
