WITH used AS (
  SELECT m.namespace, m.name AS pod, sum(axiom_quantity(c->'usage'->>'memory')) AS bytes
    FROM k8s.metrics_k8s_io_pods m, jsonb_array_elements(m.containers) c
   GROUP BY 1, 2),
requested AS (
  SELECT p.namespace, p.name AS pod,
         r.metadata->'ownerReferences'->0->>'name' AS deployment,
         sum(axiom_quantity(c->'resources'->'requests'->>'memory')) AS bytes
    FROM k8s.core_pods p
    JOIN k8s.apps_replicasets r
      ON r.namespace = p.namespace AND r.name = p.metadata->'ownerReferences'->0->>'name',
         jsonb_array_elements(p.spec->'containers') c
   GROUP BY 1, 2, 3),
ratio AS (
  SELECT q.namespace, q.deployment, round(100 * sum(u.bytes) / sum(q.bytes)) AS pct
    FROM requested q JOIN used u USING (namespace, pod)
   GROUP BY 1, 2
  HAVING sum(q.bytes) > 0)
UPDATE k8s.apps_deployments d
   SET annotations = coalesce(d.annotations, '{}')
                     || jsonb_build_object('axiom/memory-used-pct', r.pct::text)
  FROM ratio r
 WHERE d.namespace = r.namespace AND d.name = r.deployment AND r.pct < 20
RETURNING d.namespace, d.name, r.pct;
