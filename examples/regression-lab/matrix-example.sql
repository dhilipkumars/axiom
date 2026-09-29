-- An example matrix: one Postgres build, two shared_buffers settings.
--   psql -v image=postgres:17-bookworm -f matrix-example.sql
\set ON_ERROR_STOP on
INSERT INTO lab.matrix (run, image, settings) VALUES
  ('buffers-16mb', :'image', '-c shared_buffers=16MB'),
  ('buffers-256mb', :'image', '-c shared_buffers=256MB')
ON CONFLICT (run) DO NOTHING;
