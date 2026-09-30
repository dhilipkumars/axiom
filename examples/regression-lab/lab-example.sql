-- An example lab: has Postgres 17 or 18 regressed against 16 on pgbench's
-- default workload? The same cluster three times, differing only in its
-- major version, each with the same 500m CPU limit, and every run measured by
-- the same pgbench 18 client. A fourth run names a cluster that was never
-- created, to show what a mistake looks like.
--   psql -f lab-example.sql
\set ON_ERROR_STOP on
INSERT INTO lab.clusters (name, image, cpu, memory, parameters) VALUES
  ('pg16', 'ghcr.io/cloudnative-pg/postgresql:16.15', '500m', '512Mi', '{"shared_buffers": "128MB"}'),
  ('pg17', 'ghcr.io/cloudnative-pg/postgresql:17.11', '500m', '512Mi', '{"shared_buffers": "128MB"}'),
  ('pg18', 'ghcr.io/cloudnative-pg/postgresql:18.4',  '500m', '512Mi', '{"shared_buffers": "128MB"}')
ON CONFLICT (name) DO NOTHING;
INSERT INTO lab.runs (run, cluster, clients, seconds, client_image) VALUES
  ('pg16-8-clients',  'pg16',       8, 60, 'ghcr.io/cloudnative-pg/postgresql:18.4'),
  ('pg17-8-clients',  'pg17',       8, 60, 'ghcr.io/cloudnative-pg/postgresql:18.4'),
  ('pg18-8-clients',  'pg18',       8, 60, 'ghcr.io/cloudnative-pg/postgresql:18.4'),
  ('missing-cluster', 'pg-missing', 8, 60, 'ghcr.io/cloudnative-pg/postgresql:18.4')
ON CONFLICT (run) DO NOTHING;
