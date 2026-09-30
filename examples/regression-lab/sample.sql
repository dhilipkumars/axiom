-- Record what the lab's Postgres and pgbench Pods are using, once. Run it
-- repeatedly while benchmarks run, for example with psql's \watch:
--
--   psql -v namespace=regression-lab
--   => \i sample.sql
--   => \watch 5
--
-- \watch repeats the last statement until interrupted; it needs an
-- interactive session, since psql -f runs a file once and exits.
--
-- metrics-server refreshes each Pod every 15 seconds or so, and each sample is
-- keyed on its own timestamp, so sampling faster than that records nothing
-- twice. PodMetrics carry no labels of their own, so each is joined to its Pod
-- for them. Only CloudNativePG's database Pods (cnpg.io/podRole=instance),
-- not its one-off initdb Job, count as Postgres.
INSERT INTO lab.usage (namespace, pod, sampled_at, window_s, cluster, role, cpu_cores, memory)
SELECT m.namespace, m.name, m."timestamp",
       -- `window` is a Go duration such as "15.012s".
       CASE WHEN m."window" ~ '^[0-9.]+s$' THEN rtrim(m."window", 's')::numeric END,
       coalesce(p.labels->>'cnpg.io/cluster', p.labels->>'axiom-lab/cluster'),
       CASE WHEN p.labels ? 'axiom-lab/run' THEN 'pgbench' ELSE 'postgres' END,
       sum(axiom_quantity(c->'usage'->>'cpu')),
       sum(axiom_quantity(c->'usage'->>'memory'))
  FROM lab.pod_metrics m
  JOIN lab.pods p ON p.namespace = m.namespace AND p.name = m.name
  CROSS JOIN LATERAL jsonb_array_elements(m.containers) c
 WHERE m.namespace = :'namespace'
   AND p.namespace = :'namespace'
   AND (p.labels->>'cnpg.io/podRole' = 'instance' OR p.labels ? 'axiom-lab/run')
 GROUP BY 1, 2, 3, 4, 5, 6
ON CONFLICT (namespace, pod, sampled_at) DO NOTHING;
