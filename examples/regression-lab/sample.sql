-- Record each running benchmark's resource use once. Run it repeatedly while
-- the matrix runs, for example with psql's \watch:
--
--   psql -v namespace=regression-lab
--   => \i sample.sql
--   => \watch 5
--
-- metrics-server copies a Pod's labels onto its PodMetrics, which is how a
-- sample finds its run. axiom_quantity() turns "250m" and "64Mi" into numbers
-- that can be summed and compared.
INSERT INTO lab.usage (namespace, run, cpu_cores, memory)
SELECT m.namespace, m.labels->>'axiom-lab/run',
       sum(axiom_quantity(c->'usage'->>'cpu')),
       sum(axiom_quantity(c->'usage'->>'memory'))
  FROM lab.pod_metrics m, jsonb_array_elements(m.containers) c
 WHERE m.namespace = :'namespace' AND m.labels ? 'axiom-lab/run'
 GROUP BY 1, 2;
