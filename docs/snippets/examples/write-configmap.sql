UPDATE k8s.core_configmaps
   SET data = data || '{"LOG_LEVEL":"debug"}'
 WHERE namespace = 'shop' AND name = 'checkout-config'
RETURNING name, data;
