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
