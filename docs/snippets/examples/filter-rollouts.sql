SELECT namespace, name, replicas, ready_replicas
  FROM k8s.apps_deployments
 WHERE coalesce(ready_replicas, 0) < replicas;
