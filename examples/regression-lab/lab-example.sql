-- An example lab: has Postgres 17 or 18 regressed against 16 on pgbench's
-- default workload? The same cluster three times, differing only in its
-- major version, each with 2 CPUs, and every run measured by the same
-- pgbench 18 client. The runs are queued in this order and run one at a time.
--   psql -f lab-example.sql
\set ON_ERROR_STOP on
INSERT INTO lab.clusters (name, image, cpu, memory, parameters) VALUES
  ('pg16', 'ghcr.io/cloudnative-pg/postgresql:16.15', '2', '512Mi', '{"shared_buffers": "128MB"}'),
  ('pg17', 'ghcr.io/cloudnative-pg/postgresql:17.11', '2', '512Mi', '{"shared_buffers": "128MB"}'),
  ('pg18', 'ghcr.io/cloudnative-pg/postgresql:18.4',  '2', '512Mi', '{"shared_buffers": "128MB"}')
ON CONFLICT (name) DO NOTHING;
INSERT INTO lab.runs (run, cluster, clients, seconds, client_image) VALUES
  ('pg16-8-clients', 'pg16', 8, 60, 'ghcr.io/cloudnative-pg/postgresql:18.4'),
  ('pg17-8-clients', 'pg17', 8, 60, 'ghcr.io/cloudnative-pg/postgresql:18.4'),
  ('pg18-8-clients', 'pg18', 8, 60, 'ghcr.io/cloudnative-pg/postgresql:18.4')
ON CONFLICT (run) DO NOTHING;
