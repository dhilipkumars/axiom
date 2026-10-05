SELECT name, phase, node, creation_timestamp
  FROM k8s.pods
 WHERE namespace = 'kube-system'
 ORDER BY name;
