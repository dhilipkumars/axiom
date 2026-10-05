INSERT INTO k8s.core_configmaps (namespace, name, raw) VALUES ('shop', 'flags',
  '{"metadata":{"labels":{"team":"payments"}},"data":{"NEW_CHECKOUT":"off"}}')
RETURNING name, labels, data;

-- a copy under a new name, with data overridden
INSERT INTO k8s.core_configmaps (namespace, name, data, raw)
  SELECT 'shop', 'flags-canary', '{"NEW_CHECKOUT":"on"}', raw
    FROM k8s.core_configmaps WHERE namespace = 'shop' AND name = 'flags'
RETURNING name, labels, data;
