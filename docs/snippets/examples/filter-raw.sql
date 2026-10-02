SELECT d.namespace, d.name AS deployment, c->>'name' AS container
  FROM k8s.apps_deployments d,
       jsonb_array_elements(d.raw->'spec'->'template'->'spec'->'containers') c
 WHERE c->'resources'->'limits'->'memory' IS NULL;
