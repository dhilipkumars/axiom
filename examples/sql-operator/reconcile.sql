-- One reconcile pass: the whole operator.
--
-- Level-triggered and idempotent. It compares every ConfigMap in an owned
-- namespace against sqlop.owners and writes only the ones that differ, so
-- running it twice changes nothing the second time, and running it after a
-- missed notification still converges.
--
-- Not atomic across objects: each row is its own write to the API server. If
-- a conflicting writer makes the fifth one fail (SQLSTATE 40001), the first
-- four are already written and the statement reports the failure; the next
-- pass picks up the rest.
UPDATE sqlop.configmaps c
   SET labels = coalesce(c.labels, '{}'::jsonb) || jsonb_build_object('team', o.team)
  FROM sqlop.owners o
 WHERE c.namespace = o.namespace
   AND c.labels->>'team' IS DISTINCT FROM o.team;
