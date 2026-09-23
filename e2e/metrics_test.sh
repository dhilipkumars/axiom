#!/usr/bin/env bash
# Metrics and events E2E: with a kind cluster running metrics-server, prove
# that `metrics.k8s.io` and the two events API groups reach SQL through the
# gateway's own ServiceAccount, that the numbers match `kubectl top`, and that
# axiom_quantity() turns Kubernetes quantity strings into numbers you can
# compare and sum.
#
# The ServiceAccount is the point. Local development uses a cluster-admin
# kubeconfig, so metrics appearing there proves nothing about a real
# deployment -- it is the wildcard read rule in deploy/k8s/gateway-rbac.yaml
# that makes an API group installed *after* the gateway was written visible
# without anyone editing RBAC, and only an in-cluster gate tests that.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
E2E_GATEWAY_MODE=incluster

source "$here/lib/stack.sh"
source "$here/lib/kind.sh"

E2E_COMPOSE_OVERLAYS="${E2E_COMPOSE_OVERLAYS:-} $E2E_ROOT/deploy/compose/docker-compose.kind.yml $E2E_ROOT/deploy/compose/docker-compose.incluster.yml"
NS="axiom-e2e"

kind_up
e2e_on_teardown kind_down
kind_metrics_server
stack_up
# metrics.k8s.io names its resources `pods` and `nodes`, the same as the core
# group, so both are served here: the gate asserts the disambiguated names.
kind_deploy_gateway "pods,events,pods.metrics.k8s.io,nodes.metrics.k8s.io"

log "applying fixture pods and waiting for Ready"
kind_apply "$here/fixtures/pods.yaml"
kind_wait_pods "$NS"

log "defining the server and importing"
psql_axiom "CREATE EXTENSION IF NOT EXISTS axiom;"
psql_axiom "DROP SERVER IF EXISTS kind CASCADE;"
psql_axiom "CREATE SERVER kind FOREIGN DATA WRAPPER axiom_fdw OPTIONS (endpoint '$E2E_GATEWAY_ENDPOINT', ca_cert '/certs/ca.crt', rpc_timeout_secs '15');"
psql_axiom "CREATE SCHEMA IF NOT EXISTS k8s;"
psql_axiom "IMPORT FOREIGN SCHEMA k8s FROM SERVER kind INTO k8s;"

log "the colliding names are disambiguated, not dropped"
got="$(psql_axiom "SELECT string_agg(foreign_table_name, ',' ORDER BY foreign_table_name)
                     FROM information_schema.foreign_tables
                    WHERE foreign_table_schema = 'k8s'
                      AND foreign_table_name LIKE 'pods%';")"
[[ "$got" == "pods_core,pods_metrics_k8s_io" ]] \
  || fail "expected pods_core,pods_metrics_k8s_io from the collision rule; got '$got'"

log "metrics reach SQL through the gateway ServiceAccount, and cover the fixture pods"
want="$(kubectl_e2e top pods -n "$NS" --no-headers 2>/dev/null | awk '{print $1}' | sort | tr '\n' ',')"
got="$(psql_axiom "SELECT string_agg(name, ',' ORDER BY name) || ','
                     FROM k8s.pods_metrics_k8s_io WHERE namespace = '$NS';")"
[[ -n "$want" ]] || fail "kubectl top returned nothing for $NS; the gate cannot compare"
[[ "$got" == "$want" ]] || fail $'pod metrics mismatch\n--- postgres: '"$got"$'\n--- kubectl:  '"$want"
echo "$got"

log "node metrics are present too"
got="$(psql_axiom "SELECT count(*) FROM k8s.nodes_metrics_k8s_io;")"
[[ "$got" -ge 1 ]] || fail "expected at least one node in nodes_metrics_k8s_io, got '$got'"

log "axiom_quantity turns quantity strings into numbers that compare and sum"
# Exactness, not approximation: 100m is 0.1, and Mi is 1024-based, not 1000.
got="$(psql_axiom "SELECT axiom_quantity('100m')::text || '|' || axiom_quantity('128Mi')::text || '|' || axiom_quantity('49903n')::text;")"
[[ "$got" == "0.100|134217728|0.000049903" ]] \
  || fail "axiom_quantity conversions wrong: '$got'"
# NULL rather than an error, so one malformed field cannot fail a cluster-wide query.
got="$(psql_axiom "SELECT coalesce(axiom_quantity('nonsense')::text, 'NULL');")"
[[ "$got" == "NULL" ]] || fail "axiom_quantity('nonsense') should be NULL, got '$got'"

log "the numbers are usable: real memory usage summed across the fixture pods"
got="$(psql_axiom "SELECT sum(axiom_quantity(c->'usage'->>'memory')) > 0
                     FROM k8s.pods_metrics_k8s_io m, jsonb_array_elements(m.containers) c
                    WHERE m.namespace = '$NS';")"
[[ "$got" == "t" ]] || fail "summed memory usage was not positive: '$got'"

log "events reach SQL from both API groups"
for t in events_core events_events_k8s_io; do
  psql_axiom "SELECT 1 FROM k8s.$t LIMIT 1;" >/dev/null || fail "$t is not queryable"
done

log "the differentiating query runs: usage joined to spec and events in one statement"
psql_axiom "
  WITH usage AS (
    SELECT m.namespace, m.name AS pod,
           sum(axiom_quantity(c->'usage'->>'memory')) AS mem_used
      FROM k8s.pods_metrics_k8s_io m, jsonb_array_elements(m.containers) c
     GROUP BY 1,2)
  SELECT count(*)
    FROM k8s.pods_core p
    LEFT JOIN usage u ON u.namespace = p.namespace AND u.pod = p.name
   WHERE p.namespace = '$NS';" >/dev/null || fail "the cross-source join failed"

log "PASS"
