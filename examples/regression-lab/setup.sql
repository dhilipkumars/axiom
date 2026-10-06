-- The regression lab's tables. Run once:
--   psql -v server=<your axiom server> -f setup.sql
\set ON_ERROR_STOP on

CREATE SCHEMA IF NOT EXISTS lab;

-- The Postgres clusters under test, one CloudNativePG Cluster per row
-- (clusters.sql). Vary what you want to compare: the CPU and memory limits,
-- the server parameters, the image.
CREATE TABLE IF NOT EXISTS lab.clusters (
  name       text PRIMARY KEY CHECK (length(name) <= 40 AND name ~ '^[a-z0-9]([-a-z0-9]*[a-z0-9])?$'),
  image      text NOT NULL DEFAULT 'ghcr.io/cloudnative-pg/postgresql:18.4',
  cpu        text NOT NULL,              -- the limit: '500m', '2'
  memory     text NOT NULL,              -- request and limit: '512Mi'
  storage    text NOT NULL DEFAULT '1Gi',
  parameters jsonb NOT NULL DEFAULT '{}' -- postgresql.conf, as strings
);

-- The benchmark runs, one pgbench Job per row, started one at a time in the
-- order they were queued (launch.sql). `cluster` names a row of lab.clusters.
CREATE TABLE IF NOT EXISTS lab.runs (
  -- It names the Job, bench-<run>, which the Job controller copies into a
  -- label value, and label values stop at 63 characters.
  run     text PRIMARY KEY CHECK (length(run) <= 57 AND run ~ '^[a-z0-9]([-a-z0-9]*[a-z0-9])?$'),
  cluster text NOT NULL,
  clients int  NOT NULL DEFAULT 4 CHECK (clients > 0),
  seconds int  NOT NULL DEFAULT 60 CHECK (seconds > 0),
  scale   int  NOT NULL DEFAULT 5 CHECK (scale > 0),
  -- The pgbench client's image; the cluster's own when NULL. Comparing
  -- servers, fix it, so one pgbench version measures them all.
  client_image text,
  queued_at    timestamptz NOT NULL DEFAULT clock_timestamp()
);

-- What the benchmarked Pods used, as metrics-server reported it (sample.sql).
-- metrics.k8s.io reports only running Pods, so this has to be recorded while
-- the runs are going. One row per Pod per metrics sample: the sample's own
-- timestamp is the key, so sampling faster than metrics-server refreshes
-- records nothing twice.
CREATE TABLE IF NOT EXISTS lab.usage (
  namespace  text NOT NULL,
  pod        text NOT NULL,
  sampled_at timestamptz NOT NULL,  -- the end of the sample's window
  window_s   numeric,               -- how long the window was, in seconds
  cluster    text,
  role       text NOT NULL CHECK (role IN ('postgres', 'pgbench')),
  cpu_cores  numeric,
  memory     numeric,
  PRIMARY KEY (namespace, pod, sampled_at)
);

CREATE FOREIGN TABLE IF NOT EXISTS lab.pg_clusters (
  name text, namespace text, status jsonb, raw jsonb
) SERVER :"server" OPTIONS (resource 'clusters', group 'postgresql.cnpg.io', version 'v1', kind 'Cluster');

CREATE FOREIGN TABLE IF NOT EXISTS lab.jobs (
  name text, namespace text, labels jsonb, status jsonb, raw jsonb
) SERVER :"server" OPTIONS (resource 'jobs', group 'batch', version 'v1', kind 'Job');

CREATE FOREIGN TABLE IF NOT EXISTS lab.pods (
  name text, namespace text, creation_timestamp timestamptz, labels jsonb, status jsonb, raw jsonb
) SERVER :"server" OPTIONS (resource 'pods');

-- `timestamp` and `window` are the sample's end and length. `window` is a
-- reserved word in SQL, hence the quotes.
CREATE FOREIGN TABLE IF NOT EXISTS lab.pod_metrics (
  name text, namespace text, "timestamp" timestamptz, "window" text, containers jsonb, raw jsonb
) SERVER :"server" OPTIONS (resource 'pods', group 'metrics.k8s.io', version 'v1beta1', kind 'PodMetrics');

-- Every run: where it has got to, its result, and what the Postgres under test
-- used while it ran. `SELECT * FROM lab.results;` is the whole report.
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
--
-- The namespace is the one rbac.yaml grants, written out because a view
-- cannot take a psql variable.
CREATE OR REPLACE VIEW lab.results AS
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
   WHERE p.namespace = 'regression-lab'
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
      ON u.namespace = 'regression-lab'
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
