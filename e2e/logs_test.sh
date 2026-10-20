#!/usr/bin/env bash
# Pod logs E2E (#105): axiom_pod_log against a real cluster. Known lines come
# back as rows; the container kubectl would pick is the one read; a crash's
# output is reachable through `previous`; a pod with no such log is no rows,
# so a LATERAL query over many pods survives one; the newest bytes are kept
# when the window is larger than asked for. Off unless the gateway is started
# with --serve-pod-logs, and reached by a role only through a wrapper that
# fixes the namespace.
#
# It also runs the queries the "Reading pod logs" page includes from
# docs/snippets/examples/ and prints their tables, which the page and the
# README quote, as the docs gate does for the other examples.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
E2E_GATEWAY_MODE=incluster

source "$here/lib/stack.sh"
source "$here/lib/kind.sh"
source "$here/lib/examples.sh"

E2E_COMPOSE_OVERLAYS="${E2E_COMPOSE_OVERLAYS:-} $E2E_ROOT/deploy/compose/docker-compose.kind.yml $E2E_ROOT/deploy/compose/docker-compose.incluster.yml"
NS="shop"
Q="$E2E_ROOT/docs/snippets/examples"
BAD="$(printf '\xef\xbf\xbd')" # U+FFFD, what a byte that is not UTF-8 becomes

kind_up
e2e_on_teardown kind_down
stack_up

log "a frontend that logs requests, and a payments pod that panicked once"
fixtures_down() {
  kubectl_e2e delete -f "$E2E_ROOT/e2e/fixtures/logs.yaml" --ignore-not-found --wait=false >/dev/null 2>&1 || true
}
e2e_on_teardown fixtures_down
# The docs gate's shop may still be terminating on a shared cluster.
kubectl_e2e delete namespace "$NS" --ignore-not-found --wait=true >/dev/null
kubectl_e2e apply -f "$E2E_ROOT/e2e/fixtures/logs.yaml" >/dev/null || fail "apply logs fixture"
kubectl_e2e -n "$NS" wait --for=condition=Ready pod/frontend --timeout=120s >/dev/null \
  || fail "frontend did not become ready"
# Panicked once, restarted, and running its second life.
deadline=$((SECONDS + 120))
until kubectl_e2e -n "$NS" logs payments 2>/dev/null | grep -q '^payments v1.4 ready'; do
  (( SECONDS < deadline )) || fail "payments never restarted into its second life"
  sleep 2
done
# The app container prints and then sleeps; wait until its lines are written.
deadline=$((SECONDS + 60))
until kubectl_e2e -n "$NS" logs frontend -c app 2>/dev/null | grep -q '^GET /search'; do
  (( SECONDS < deadline )) || fail "frontend never printed"
  sleep 1
done

log "server prod and schema k8s, as the examples pages assume"
psql_axiom "CREATE EXTENSION IF NOT EXISTS axiom;"
psql_axiom "DROP SERVER IF EXISTS prod CASCADE;"
psql_axiom "DROP SCHEMA IF EXISTS k8s, tenant CASCADE;" >/dev/null
psql_axiom "DROP ROLE IF EXISTS log_reader;" >/dev/null
psql_axiom "CREATE SERVER prod FOREIGN DATA WRAPPER axiom_fdw OPTIONS (endpoint '$E2E_GATEWAY_ENDPOINT', ca_cert '/certs/ca.crt', rpc_timeout_secs '15');"

log "a gateway started without --serve-pod-logs refuses"
E2E_SERVE_POD_LOGS=0 kind_deploy_gateway "pods,configmaps"
out="$(psql_axiom "SELECT * FROM axiom_pod_log('prod', '$NS', 'frontend');" 2>&1 || true)"
grep -q -- "--serve-pod-logs" <<<"$out" || fail "logs were served without the flag: $out"
echo "refused: $out"

log "with the flag, the default container's lines come back as rows"
E2E_SERVE_POD_LOGS=1 kind_deploy_gateway "pods,configmaps"
psql_axiom "CREATE SCHEMA k8s;"
psql_axiom "IMPORT FOREIGN SCHEMA k8s LIMIT TO (core_pods) FROM SERVER prod INTO k8s;"
got="$(psql_axiom "SELECT container_name || ':' || line_no || ':' || line || ':' || (ts IS NOT NULL) || ':' || truncated
                     FROM axiom_pod_log('prod', '$NS', 'frontend') ORDER BY line_no;")"
want="app:1:listening on :8080:true:false
app:2:GET /cart 200 12ms:true:false
app:3:GET /search?q=caf$BAD 200 9ms:true:false"
[[ "$got" == "$want" ]] || fail "frontend's log:
got:
$got
want:
$want"
echo "$got"
echo "the annotation chose app over envoy, and the Latin-1 byte became U+FFFD"

log "a named container, and the bounds"
got="$(psql_axiom "SELECT line FROM axiom_pod_log('prod', '$NS', 'frontend', container => 'envoy');")"
[[ "$got" == "envoy proxy started" ]] || fail "envoy's log: '$got'"
got="$(psql_axiom "SELECT string_agg(line, '|' ORDER BY line_no) FROM axiom_pod_log('prod', '$NS', 'frontend', tail_lines => 1);")"
[[ "$got" == "GET /search?q=caf$BAD 200 9ms" ]] || fail "tail_lines => 1: '$got'"
# Room for the last line only: the newest bytes are kept, and the result says it is cut.
got="$(psql_axiom "SELECT string_agg(line || ':' || truncated, '|' ORDER BY line_no)
                     FROM axiom_pod_log('prod', '$NS', 'frontend', max_bytes => 30);")"
[[ "$got" == "GET /search?q=caf$BAD 200 9ms:true" ]] || fail "max_bytes => 30: '$got'"
got="$(psql_axiom "SELECT count(*) FROM axiom_pod_log('prod', '$NS', 'frontend', since => now() + interval '1 hour');")"
[[ "$got" == "0" ]] || fail "since in the future returned $got rows"
echo "container, tail_lines, max_bytes and since all applied"

log "a crash's output is in the previous instance"
got="$(psql_axiom "SELECT string_agg(line, '|' ORDER BY line_no) FROM axiom_pod_log('prod', '$NS', 'payments', previous => true);")"
[[ "$got" == "payments v1.4 starting|panic: dial tcp 10.96.0.15:5432: connect: connection refused" ]] \
  || fail "payments' previous log: '$got'"
got="$(psql_axiom "SELECT string_agg(line, '|' ORDER BY line_no) FROM axiom_pod_log('prod', '$NS', 'payments');")"
[[ "$got" == "payments v1.4 ready" ]] || fail "payments' current log: '$got'"
got="$(psql_axiom "SELECT count(*) FROM axiom_pod_log('prod', '$NS', 'frontend', previous => true);")"
[[ "$got" == "0" ]] || fail "a container that never restarted has no previous log, got $got rows"
echo "previous reads the panic, the current instance its second life; a container that never restarted is no rows, not an error"

log "the queries the Reading pod logs page and the README show"
show() {
  echo
  echo "=== docs/snippets/examples/$1"
  psql_table "$Q/$1" || fail "$1 failed"
}
show pod-log.sql
psql_file "$Q/pod-log.sql" | grep -q "^app|3|GET /search" || fail "pod-log.sql did not read frontend's app container"
show pod-log-crashes.sql
got="$(psql_file "$Q/pod-log-crashes.sql" | cut -d'|' -f1,3 | paste -sd'#' -)"
[[ "$got" == "payments|payments v1.4 starting#payments|panic: dial tcp 10.96.0.15:5432: connect: connection refused" ]] \
  || fail "pod-log-crashes.sql: '$got'"

log "a missing pod is an error that names it"
out="$(psql_axiom "SELECT * FROM axiom_pod_log('prod', '$NS', 'nope');" 2>&1 || true)"
grep -q "pod $NS/nope not found" <<<"$out" || fail "missing pod: $out"
echo "$out"

log "a role reaches logs only through a wrapper that fixes the namespace"
psql_axiom "CREATE ROLE log_reader;" >/dev/null
out="$(psql_axiom "SET ROLE log_reader; SELECT * FROM axiom_pod_log('prod', '$NS', 'frontend');" 2>&1 || true)"
grep -q "permission denied for function axiom_pod_log" <<<"$out" || fail "PUBLIC could execute it: $out"
# The guide's recipe.
psql_axiom "
CREATE SCHEMA tenant;
CREATE FUNCTION tenant.pod_log(pod text, tail_lines integer DEFAULT 100)
  RETURNS TABLE (container_name text, line_no integer, ts timestamptz, line text, truncated boolean)
  LANGUAGE sql STABLE SECURITY DEFINER
  SET search_path = pg_catalog, pg_temp
  AS \$\$ SELECT * FROM public.axiom_pod_log('prod', '$NS', pod, tail_lines => tail_lines) \$\$;
REVOKE ALL ON FUNCTION tenant.pod_log FROM PUBLIC;
GRANT USAGE ON SCHEMA tenant TO log_reader;
GRANT EXECUTE ON FUNCTION tenant.pod_log TO log_reader;
" >/dev/null
out="$(psql_axiom "SET ROLE log_reader; SELECT * FROM tenant.pod_log('frontend');" 2>&1)" \
  || fail "the wrapper failed for its own namespace: $out"
grep -q "listening on :8080" <<<"$out" || fail "the wrapper did not read frontend: $out"
echo "the wrapper reads $NS's logs, and takes no server or namespace to point elsewhere"

psql_axiom "DROP SCHEMA tenant, k8s CASCADE; DROP ROLE log_reader;" >/dev/null 2>&1 || true

log "POD LOGS E2E PASSED"
