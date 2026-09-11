#!/usr/bin/env bash
# Watch E2E (Phase 3 in docs/PLAN.md): a `cache_mode 'watch'` foreign table is
# served from the shared-memory cache fed by the gateway's Subscribe stream.
# Proves: one initial LIST only; kubectl-side changes appear without new LISTs;
# gateway loss → DEGRADED with stale-but-served reads; gateway return → resume
# from bookmark (no relist) with changes made during the outage; NOTIFY payloads.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$here/lib/stack.sh"
source "$here/lib/kind.sh"

E2E_COMPOSE_OVERLAYS="${E2E_COMPOSE_OVERLAYS:-} $E2E_ROOT/deploy/compose/docker-compose.kind.yml"
NS="axiom-e2e"

kind_up
e2e_on_teardown kind_down
stack_up

# --- helpers ---------------------------------------------------------------------------
watch_state() { psql_axiom "SELECT state FROM axiom_watch_status() WHERE resource = 'pods' AND namespace = '$NS';"; }
wait_state() { # wait_state STATE TIMEOUT
  local want="$1" timeout="$2" deadline=$((SECONDS + $2)) got=""
  until got="$(watch_state 2>/dev/null)" && [[ "$got" == "$want" ]]; do
    (( SECONDS < deadline )) || fail "watch did not reach $want within ${timeout}s (last: '$got'); status: $(psql_axiom "SELECT row_to_json(s) FROM axiom_watch_status() s;" 2>&1)"
    sleep 1
  done
}
sql_names() { psql_axiom "SELECT string_agg(name, ',' ORDER BY name) FROM k8s_pods_live WHERE namespace = '$NS';"; }
k8s_names() { kubectl_e2e -n "$NS" get pods --no-headers -o custom-columns=':metadata.name' 2>/dev/null | sort | paste -sd, - ; }
wait_sql_names() { # wait_sql_names EXPECTED TIMEOUT
  local want="$1" deadline=$((SECONDS + $2)) got=""
  until got="$(sql_names 2>/dev/null)" && [[ "$got" == "$want" ]]; do
    (( SECONDS < deadline )) || fail "SQL never showed '$want' (last: '$got')"
    sleep 1
  done
}
list_count() { stack_logs "$E2E_SVC_GATEWAY" | grep -c '"msg":"list".*"gvk":"/v1, Kind=Pod"' || true; }
run_pod() { kubectl_e2e -n "$NS" run "$1" --image=registry.k8s.io/pause:3.10 --restart=Never >/dev/null; }

log "namespace and DDL"
kubectl_e2e create namespace "$NS" --dry-run=client -o yaml | kubectl_e2e apply -f - >/dev/null
kubectl_e2e -n "$NS" delete pods --all --ignore-not-found --wait=true >/dev/null
psql_axiom "CREATE EXTENSION IF NOT EXISTS axiom;"
psql_axiom "DROP SERVER IF EXISTS kind CASCADE;"
psql_axiom "CREATE SERVER kind FOREIGN DATA WRAPPER axiom_fdw OPTIONS (endpoint '$E2E_GATEWAY_ENDPOINT', ca_cert '/certs/ca.crt', rpc_timeout_secs '10');"
psql_axiom "CREATE FOREIGN TABLE k8s_pods_live (name text, namespace text, phase text, node text, raw jsonb) SERVER kind OPTIONS (resource 'pods', cache_mode 'watch');"
run_pod seed-0; run_pod seed-1
kind_wait_pods "$NS"

log "first scan warms the watch (served on demand), then the subscription becomes ACTIVE"
got="$(sql_names)"; [[ "$got" == "seed-0,seed-1" ]] || fail "warm scan returned '$got'"
wait_state ACTIVE 60
L0="$(list_count)"
[[ "$L0" -ge 1 ]] || fail "expected at least one on-demand LIST before the watch was active, got $L0"
[[ "$(stack_logs "$E2E_SVC_GATEWAY" | grep -c '"msg":"subscribe_list"')" == "1" ]] || fail "expected exactly one subscribe_list"
echo "on-demand LISTs so far: $L0"

log "cached scans issue no LIST RPCs"
for _ in 1 2 3; do sql_names >/dev/null; done
[[ "$(list_count)" == "$L0" ]] || fail "cached scans issued LISTs: $(list_count) != $L0"

log "kubectl-side create/delete is reflected via the stream, still without LISTs"
run_pod watch-1
wait_sql_names "seed-0,seed-1,watch-1" 30
kubectl_e2e -n "$NS" delete pod watch-1 --wait=false >/dev/null
wait_sql_names "seed-0,seed-1" 60
[[ "$(list_count)" == "$L0" ]] || fail "live updates issued LISTs: $(list_count) != $L0"
[[ "$(sql_names)" == "$(k8s_names)" ]] || fail "SQL '$(sql_names)' != kubectl '$(k8s_names)'"

log "gateway down: subscription DEGRADED, stale cache still served with a WARNING"
compose stop "$E2E_SVC_GATEWAY" >/dev/null 2>&1
wait_state DEGRADED 60
out="$(compose exec -T "$E2E_SVC_POSTGRES" psql -U "$E2E_PG_USER" -d "$E2E_PG_DB" -At -c "SELECT count(*) FROM k8s_pods_live WHERE namespace = '$NS';" 2>&1)"
grep -q "WARNING:  axiom: serving STALE data" <<<"$out" || fail "no STALE warning on degraded read: $out"
grep -qx "2" <<<"$out" || fail "stale read did not return the cached rows: $out"

log "a change during the outage is picked up on resume from the bookmark, without a relist"
run_pod watch-2
compose start "$E2E_SVC_GATEWAY" >/dev/null 2>&1
wait_state ACTIVE 120
wait_sql_names "seed-0,seed-1,watch-2" 60
[[ "$(list_count)" == "$L0" ]] || fail "resume issued LISTs: $(list_count) != $L0"
# `compose stop/start` keeps the container log, so the initial listing is still there: exactly one, no relist.
[[ "$(stack_logs "$E2E_SVC_GATEWAY" | grep -c '"msg":"subscribe_list"')" == "1" ]] || fail "resume caused a relist (subscribe_list count != 1)"
stack_logs "$E2E_SVC_GATEWAY" | grep '"msg":"subscribe"' | tail -1 | grep -q '"resource_version":"[0-9]' || fail "reconnect did not carry a resume bookmark"
[[ "$(sql_names)" == "$(k8s_names)" ]] || fail "after resync SQL '$(sql_names)' != kubectl '$(k8s_names)'"

log "LISTEN axiom_events receives a NOTIFY for a kubectl-side change"
( sleep 3; run_pod watch-3 ) &
out="$(printf 'LISTEN axiom_events;\nSELECT pg_sleep(10);\nSELECT 1;\n' | compose exec -T "$E2E_SVC_POSTGRES" psql -U "$E2E_PG_USER" -d "$E2E_PG_DB" -At 2>&1)"
wait
grep -q 'Asynchronous notification "axiom_events" with payload' <<<"$out" || fail "no notification received: $out"
grep -q '"name":"watch-3"' <<<"$out" || fail "payload lacks the pod name: $out"
grep -q '"type":"ADDED"' <<<"$out" || fail "payload lacks the event type: $out"
grep -q '"resource":"pods"' <<<"$out" || fail "payload lacks the resource: $out"
echo "$(grep -m1 'payload' <<<"$out")"

log "cleanup"
kubectl_e2e -n "$NS" delete pods --all --ignore-not-found --wait=false >/dev/null

log "WATCH E2E PASSED"
