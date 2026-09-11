#!/usr/bin/env bash
# Pods E2E (Phase 1 in docs/PLAN.md): with a kind cluster, the gateway running
# under a pods-read-only ServiceAccount, and Postgres with the axiom extension,
# prove SELECT on a k8s_pods foreign table returns real cluster data, that
# namespace/name quals are pushed down to the gateway, that a missing pod is an
# empty result, and that a dead gateway is a SQL error rather than a crash.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$here/lib/stack.sh"
source "$here/lib/kind.sh"

# Append the kind overlay to whatever the caller layered (e.g. docker-compose.dev.yml).
E2E_COMPOSE_OVERLAYS="${E2E_COMPOSE_OVERLAYS:-} $E2E_ROOT/deploy/compose/docker-compose.kind.yml"
NS="axiom-e2e"

kind_up
e2e_on_teardown kind_down
stack_up

log "applying fixture pods and waiting for Ready"
kind_apply "$here/fixtures/pods.yaml"
kind_wait_pods "$NS"

log "defining server and foreign table"
psql_axiom "CREATE EXTENSION IF NOT EXISTS axiom;"
psql_axiom "DROP SERVER IF EXISTS kind CASCADE;"
psql_axiom "CREATE SERVER kind FOREIGN DATA WRAPPER axiom_fdw OPTIONS (endpoint '$E2E_GATEWAY_ENDPOINT', ca_cert '/certs/ca.crt', rpc_timeout_secs '10');"
psql_axiom "CREATE FOREIGN TABLE k8s_pods (name text, namespace text, phase text, node text, raw jsonb) SERVER kind OPTIONS (resource 'pods');"

log "namespace scan matches kubectl"
want="$(kubectl_e2e -n "$NS" get pods --no-headers -o custom-columns=':metadata.name,:status.phase' | awk '{print $1"|"$2}' | sort)"
got="$(psql_axiom "SELECT name || '|' || phase FROM k8s_pods WHERE namespace = '$NS' ORDER BY 1;")"
[[ "$got" == "$want" ]] || fail $'namespace scan mismatch\n--- postgres:\n'"$got"$'\n--- kubectl:\n'"$want"
echo "$got"

log "point get matches kubectl (name, node, uid via raw jsonb)"
want="$(kubectl_e2e -n "$NS" get pod web-0 -o jsonpath='{.metadata.name}|{.spec.nodeName}|{.metadata.uid}')"
got="$(psql_axiom "SELECT name || '|' || node || '|' || (raw->'metadata'->>'uid') FROM k8s_pods WHERE namespace = '$NS' AND name = 'web-0';")"
[[ "$got" == "$want" ]] || fail "point get mismatch: postgres='$got' kubectl='$want'"
echo "$got"

log "quals were pushed down to the gateway (namespace + name), not filtered locally"
glogs="$(stack_logs "$E2E_SVC_GATEWAY")"
grep -q "\"msg\":\"list\".*\"namespace\":\"$NS\",\"name\":\"web-0\",\"count\":1" <<<"$glogs" \
  || fail $'gateway log has no List with namespace+name filter for the point get\n'"$(grep '"msg":"list"' <<<"$glogs" | tail -5)"
grep -q "\"msg\":\"list\".*\"namespace\":\"$NS\",\"name\":\"\",\"count\":3" <<<"$glogs" \
  || fail "gateway log has no namespace-only List with count=3"

log "local (non-pushed) quals still apply: phase filter and label via raw"
got="$(psql_axiom "SELECT count(*) FROM k8s_pods WHERE namespace = '$NS' AND phase = 'Running';")"
[[ "$got" == "3" ]] || fail "expected 3 Running pods, got '$got'"
got="$(psql_axiom "SELECT string_agg(name, ',' ORDER BY name) FROM k8s_pods WHERE namespace = '$NS' AND raw->'metadata'->'labels'->>'app' = 'web';")"
[[ "$got" == "web-0,web-1" ]] || fail "label filter via raw jsonb returned '$got'"

log "nonexistent pod is an empty result, not an error"
got="$(psql_axiom "SELECT count(*) FROM k8s_pods WHERE namespace = '$NS' AND name = 'does-not-exist';")"
[[ "$got" == "0" ]] || fail "expected 0 rows for missing pod, got '$got'"
got="$(psql_axiom "SELECT count(*) FROM k8s_pods WHERE namespace = 'no-such-namespace';")"
[[ "$got" == "0" ]] || fail "expected 0 rows for missing namespace, got '$got'"

log "RBAC is least-privilege: the gateway identity cannot read secrets or delete pods"
# can_i VERB RESOURCE -> prints yes/no; `kubectl auth can-i` exits 1 on "no", so
# capture rather than pipe (pipefail would turn a correct "no" into a failure).
can_i() { kubectl_e2e --as="system:serviceaccount:$E2E_GATEWAY_SA_NS:$E2E_GATEWAY_SA" auth can-i "$1" "$2" -n "$NS" 2>/dev/null || true; }
[[ "$(can_i get secrets)" == "no" ]]  || fail "gateway SA can read secrets"
[[ "$(can_i delete pods)" == "no" ]]  || fail "gateway SA can delete pods"
[[ "$(can_i watch pods)" == "yes" ]]  || fail "gateway SA cannot watch pods (needed for the Phase 3 Subscribe stream)"
[[ "$(can_i list pods)" == "yes" ]]   || fail "gateway SA cannot list pods"
[[ "$(can_i get pods)" == "yes" ]]    || fail "gateway SA cannot get pods"

log "gateway down: SELECT raises fdw_unable_to_establish_connection, then recovers"
compose stop "$E2E_SVC_GATEWAY" >/dev/null 2>&1
got="$(psql_axiom "DO \$\$ BEGIN PERFORM count(*) FROM k8s_pods WHERE namespace = '$NS'; RAISE EXCEPTION 'unexpected success';
  EXCEPTION WHEN fdw_unable_to_establish_connection THEN RAISE NOTICE 'caught %', SQLSTATE; END \$\$;" 2>&1 || true)"
grep -q "caught HV00N" <<<"$got" || fail "expected SQLSTATE HV00N with gateway down, got: $got"
compose start "$E2E_SVC_GATEWAY" >/dev/null 2>&1
deadline=$((SECONDS + 30))
until got="$(psql_axiom "SELECT count(*) FROM k8s_pods WHERE namespace = '$NS';" 2>/dev/null)" && [[ "$got" == "3" ]]; do
  (( SECONDS < deadline )) || fail "scan did not recover after gateway restart (last: '$got')"
  sleep 1
done
echo "recovered: $got rows"

log "PODS E2E PASSED"
