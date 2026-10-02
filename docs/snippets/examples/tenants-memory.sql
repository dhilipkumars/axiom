SELECT t.customer,
       round(sum(axiom_quantity(c->'usage'->>'memory')) / 1024 / 1024, 1) AS mem_mib
  FROM tenants t
  JOIN k8s.metrics_k8s_io_pods m ON m.namespace = t.namespace,
       jsonb_array_elements(m.containers) c
 GROUP BY t.customer
 ORDER BY mem_mib DESC;
