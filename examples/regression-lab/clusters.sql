-- Create a CloudNativePG Cluster for every row of lab.clusters that has none.
--
--   psql -v namespace=regression-lab -f clusters.sql
--
-- A write to a custom resource, from SQL: the operator sees an ordinary
-- Cluster and builds it. Each gets one instance, the CPU limit and memory from
-- its row, and a small CPU request so a laptop cluster can schedule several.
-- CloudNativePG creates the `app` database, its owner, and a Secret,
-- <name>-app, holding that owner's password, which the benchmark Jobs use.
--
-- Wait for them before launching runs:
--   SELECT name, status->>'phase' FROM lab.pg_clusters WHERE namespace = 'regression-lab';
-- until every row says "Cluster in healthy state".
\set ON_ERROR_STOP on

INSERT INTO lab.pg_clusters (namespace, name, raw)
SELECT :'namespace', c.name, jsonb_build_object(
  'apiVersion', 'postgresql.cnpg.io/v1',
  'kind', 'Cluster',
  'spec', jsonb_build_object(
    'instances', 1,
    'imageName', c.image,
    'storage', jsonb_build_object('size', c.storage),
    'resources', jsonb_build_object(
      'requests', jsonb_build_object('cpu', '100m', 'memory', c.memory),
      'limits', jsonb_build_object('cpu', c.cpu, 'memory', c.memory)),
    'postgresql', jsonb_build_object('parameters', c.parameters)))
FROM lab.clusters c
WHERE NOT EXISTS (
  SELECT 1 FROM lab.pg_clusters p
   WHERE p.namespace = :'namespace' AND p.name = c.name);
