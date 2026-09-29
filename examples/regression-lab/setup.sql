-- The regression lab's tables. Run once:
--   psql -v server=<your axiom server> -f setup.sql
\set ON_ERROR_STOP on

CREATE SCHEMA IF NOT EXISTS lab;

-- The matrix: one row per benchmark run. `image` is the Postgres build under
-- test (anything with postgres and pgbench on its PATH), `settings` the
-- server options it starts with. Add a row, launch, compare.
CREATE TABLE IF NOT EXISTS lab.matrix (
  run      text PRIMARY KEY CHECK (run ~ '^[a-z0-9]([-a-z0-9]*[a-z0-9])?$'),
  image    text NOT NULL,
  settings text NOT NULL DEFAULT '',
  clients  int  NOT NULL DEFAULT 2,
  seconds  int  NOT NULL DEFAULT 10
);

-- Resource use sampled while runs are going (sample.sql). metrics.k8s.io
-- reports only running pods, so a finished run has nothing left to join.
CREATE TABLE IF NOT EXISTS lab.usage (
  sampled_at timestamptz NOT NULL DEFAULT now(),
  run        text NOT NULL,
  cpu_cores  numeric,
  memory     numeric
);

CREATE FOREIGN TABLE IF NOT EXISTS lab.jobs (
  name text, namespace text, labels jsonb, status jsonb, raw jsonb
) SERVER :"server" OPTIONS (resource 'jobs', group 'batch', version 'v1', kind 'Job');

CREATE FOREIGN TABLE IF NOT EXISTS lab.pods (
  name text, namespace text, labels jsonb, status jsonb, raw jsonb
) SERVER :"server" OPTIONS (resource 'pods');

-- Only needed for sample.sql, and only works where metrics-server runs.
CREATE FOREIGN TABLE IF NOT EXISTS lab.pod_metrics (
  name text, namespace text, labels jsonb, containers jsonb, raw jsonb
) SERVER :"server" OPTIONS (resource 'pods', group 'metrics.k8s.io', version 'v1beta1', kind 'PodMetrics');
