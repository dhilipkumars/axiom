SELECT status, count(*)
  FROM axiom_create_short_names('k8s')
 GROUP BY status;
