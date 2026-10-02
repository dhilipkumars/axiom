SELECT namespace, phase, count(*)
  FROM k8s.core_pods
 WHERE phase <> 'Running'
 GROUP BY namespace, phase
 ORDER BY count(*) DESC;
