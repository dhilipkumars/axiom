-- An example lab: the same Postgres with two CPU limits, one benchmark run
-- against each, and a run against a cluster that was never created.
--   psql -f lab-example.sql
\set ON_ERROR_STOP on
INSERT INTO lab.clusters (name, cpu, memory, parameters) VALUES
  ('pg-small', '500m', '512Mi', '{"shared_buffers": "128MB"}'),
  ('pg-large', '2',    '512Mi', '{"shared_buffers": "128MB"}')
ON CONFLICT (name) DO NOTHING;
INSERT INTO lab.runs (run, cluster, clients, seconds) VALUES
  ('small-8-clients', 'pg-small', 8, 60),
  ('large-8-clients', 'pg-large', 8, 60),
  ('missing-cluster', 'pg-missing', 8, 60)
ON CONFLICT (run) DO NOTHING;
