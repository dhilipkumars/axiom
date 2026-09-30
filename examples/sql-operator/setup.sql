-- The SQL operator's tables. Run once:
--   psql -v server=<your axiom server> -f setup.sql
\set ON_ERROR_STOP on

CREATE SCHEMA IF NOT EXISTS sqlop;

-- Desired state, in an ordinary table you own: which team owns each
-- namespace. The operator makes every ConfigMap in a listed namespace carry
-- a `team` label that matches it. Change a row and the cluster follows.
CREATE TABLE IF NOT EXISTS sqlop.owners (
  namespace text PRIMARY KEY,
  team      text NOT NULL
);

-- What the operator reads and writes. On-demand (the default), so every
-- reconcile sees the API server's current state, with the resourceVersion an
-- UPDATE needs for its optimistic-concurrency check.
CREATE FOREIGN TABLE IF NOT EXISTS sqlop.configmaps (
  name text, namespace text, labels jsonb, raw jsonb
) SERVER :"server" OPTIONS (resource 'configmaps');

-- Watched, so the extension's background worker keeps a live subscription
-- and sends NOTIFY axiom_events when a ConfigMap changes. The operator never
-- writes through it: a cached row can be behind the cluster, and an UPDATE
-- carrying a stale resourceVersion would only conflict. Its one job is to
-- make the loop react in seconds instead of waiting for the next sweep.
CREATE FOREIGN TABLE IF NOT EXISTS sqlop.configmaps_watched (
  name text, namespace text, raw jsonb
) SERVER :"server" OPTIONS (resource 'configmaps', cache_mode 'watch');
