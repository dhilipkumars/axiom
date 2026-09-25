#!/usr/bin/env bash
# Agent access E2E (#86): the recipe in docs/guides/agents.md, run against a
# real cluster. An agent role reads a redacted, tenant-scoped view and nothing
# else: not the tables behind it, not a write, not a foreign table of its own,
# not the watch status. Every claim the guide makes is asserted here, and the
# two controls that only look like security -- a read-only session setting
# and a statement timeout -- are shown not to be what stops a write.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
E2E_GATEWAY_MODE=incluster

source "$here/lib/stack.sh"
source "$here/lib/kind.sh"

E2E_COMPOSE_OVERLAYS="${E2E_COMPOSE_OVERLAYS:-} $E2E_ROOT/deploy/compose/docker-compose.kind.yml $E2E_ROOT/deploy/compose/docker-compose.incluster.yml"
ACME="agent-acme"
OTHER="agent-other"

kind_up
e2e_on_teardown kind_down
stack_up
kind_deploy_gateway "pods,configmaps"

log "two tenants' namespaces, each with a ConfigMap holding something sensitive"
namespaces_down() {
  kubectl_e2e delete namespace "$ACME" "$OTHER" --ignore-not-found --wait=false >/dev/null 2>&1 || true
}
e2e_on_teardown namespaces_down
for ns in "$ACME" "$OTHER"; do
  kubectl_e2e delete namespace "$ns" --ignore-not-found --wait=true >/dev/null
  kubectl_e2e create namespace "$ns" >/dev/null
  kubectl_e2e -n "$ns" create configmap app --from-literal=DB_PASSWORD=hunter2 >/dev/null
done

log "the operator's setup: import, then the agent's view and role"
psql_axiom "CREATE EXTENSION IF NOT EXISTS axiom;"
psql_axiom "DROP SERVER IF EXISTS kind CASCADE;"
psql_axiom "DROP SCHEMA IF EXISTS k8s, agent, scratch CASCADE;" >/dev/null
psql_axiom "DROP TABLE IF EXISTS tenants;" >/dev/null
psql_axiom "DROP ROLE IF EXISTS agent_role; DROP ROLE IF EXISTS writer_role;" >/dev/null
psql_axiom "CREATE SERVER kind FOREIGN DATA WRAPPER axiom_fdw OPTIONS (endpoint '$E2E_GATEWAY_ENDPOINT', ca_cert '/certs/ca.crt', rpc_timeout_secs '15');"
psql_axiom "CREATE SCHEMA k8s;"
psql_axiom "IMPORT FOREIGN SCHEMA k8s LIMIT TO (core_configmaps, core_pods) FROM SERVER kind INTO k8s;"
# The guide's recipe, verbatim.
psql_axiom "
CREATE TABLE tenants (namespace text PRIMARY KEY, customer text);
INSERT INTO tenants VALUES ('$ACME', 'Acme'), ('$OTHER', 'Other');

CREATE ROLE agent_role;
CREATE SCHEMA agent;
CREATE VIEW agent.configmaps WITH (security_barrier) AS
  SELECT namespace, name, labels, creation_timestamp
    FROM k8s.core_configmaps
   WHERE namespace IN (SELECT namespace FROM tenants WHERE customer = 'Acme');
GRANT USAGE ON SCHEMA agent TO agent_role;
GRANT SELECT ON agent.configmaps TO agent_role;
" >/dev/null

# as_agent SQL: run SQL as agent_role, printing output and errors.
as_agent() { psql_axiom "SET ROLE agent_role; $1" 2>&1 || true; }
# denied WHAT SQL: the statement must fail with a permission error.
denied() {
  local out
  out="$(as_agent "$2")"
  grep -q "permission denied" <<<"$out" || fail "the agent could $1: $out"
  echo "denied: $1"
}

log "the agent reads its tenant's objects through the view, and no one else's"
got="$(as_agent "SELECT string_agg(namespace || '/' || name, ',' ORDER BY namespace, name)
                   FROM agent.configmaps WHERE name = 'app';")"
[[ "$got" == "$ACME/app" ]] || fail "the view should show only $ACME/app; got '$got'"
echo "$got"

log "fields the view leaves out do not exist for the agent"
out="$(as_agent "SELECT data FROM agent.configmaps;")"
grep -q 'column "data" does not exist' <<<"$out" || fail "the agent reached data through the view: $out"
echo "no data column: the password is not reachable"

log "everything outside the view is refused"
denied "read the table behind the view"       "SELECT count(*) FROM k8s.core_configmaps;"
denied "read raw from the table"               "SELECT raw FROM k8s.core_configmaps LIMIT 1;"
denied "write through the view"                "UPDATE agent.configmaps SET labels = '{}' WHERE name = 'app';"
denied "delete through the view"               "DELETE FROM agent.configmaps WHERE name = 'app';"
denied "read watch status"                     "SELECT * FROM axiom_watch_status();"

log "a read-only session setting is not what stops a write"
# The agent can simply turn it off: the grant is the control.
out="$(as_agent "SET default_transaction_read_only = off; SET statement_timeout = 0;
                 BEGIN READ WRITE; DELETE FROM agent.configmaps WHERE name = 'app'; COMMIT;")"
grep -q "permission denied" <<<"$out" || fail "turning read-only off let the agent write: $out"
kubectl_e2e -n "$ACME" get configmap app >/dev/null || fail "the ConfigMap is gone"
echo "the session settings changed; the write was still refused"

log "the agent cannot define a foreign table of its own"
# Given somewhere to create one, the only thing missing is USAGE on the server,
# which is exactly what would let it read raw and bypass the view.
psql_axiom "CREATE SCHEMA scratch; GRANT USAGE, CREATE ON SCHEMA scratch TO agent_role;" >/dev/null
denied "create a foreign table" \
  "CREATE FOREIGN TABLE scratch.mine (name text, namespace text, raw jsonb) SERVER kind OPTIONS (resource 'configmaps');"

log "a writer granted one column can update it without being able to read the object"
# Column grants on the table itself: SELECT on the identity, UPDATE on data,
# nothing on raw. The FDW's UPDATE carries the object's identity and
# resourceVersion in a hidden copy of raw that Postgres adds after privileges
# are checked, so the write works without the role ever seeing raw.
psql_axiom "
CREATE ROLE writer_role;
GRANT USAGE ON SCHEMA k8s TO writer_role;
GRANT SELECT (namespace, name), UPDATE (data) ON k8s.core_configmaps TO writer_role;
" >/dev/null
out="$(psql_axiom "SET ROLE writer_role;
                   UPDATE k8s.core_configmaps SET data = '{\"DB_PASSWORD\":\"rotated\"}'
                    WHERE namespace = '$ACME' AND name = 'app';" 2>&1)" \
  || fail "a writer with UPDATE (data) and no SELECT (raw) could not update: $out"
got="$(kubectl_e2e -n "$ACME" get configmap app -o jsonpath='{.data.DB_PASSWORD}')"
[[ "$got" == "rotated" ]] || fail "the update did not reach the cluster: DB_PASSWORD is '$got'"
echo "updated through a column grant; kubectl shows the new value"
for col in raw data; do
  out="$(psql_axiom "SET ROLE writer_role; SELECT $col FROM k8s.core_configmaps LIMIT 1;" 2>&1 || true)"
  grep -q "permission denied" <<<"$out" || fail "the writer could read $col: $out"
done
echo "and it still cannot read raw or data"

psql_axiom "DROP SCHEMA agent, scratch CASCADE; DROP TABLE tenants;
            REVOKE ALL ON SCHEMA k8s FROM writer_role;
            REVOKE ALL ON k8s.core_configmaps FROM writer_role;
            DROP ROLE agent_role; DROP ROLE writer_role;" >/dev/null 2>&1 || true

log "AGENT E2E PASSED"
