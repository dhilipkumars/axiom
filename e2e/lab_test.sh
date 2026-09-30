#!/usr/bin/env bash
# Regression-lab E2E (#104, #107): examples/regression-lab run as shipped
# against a kind cluster with CloudNativePG and metrics-server. Three
# CloudNativePG clusters, Postgres 16, 17 and 18, are created from SQL; a
# pgbench Job runs against each; their resource use is sampled while the
# benchmark runs; and results.sql joins the two.
#
# Not in ALL_GATES: it installs two operators, pulls three Postgres images and
# runs minutes of benchmarks. The example-tests workflow
# (.github/workflows/example-tests.yml) runs it on demand, nightly, and on
# pull requests that touch an example.
#
#   E2E_LAB_REPORT   write the results table here as well as printing it
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Must precede the source: stack.sh resolves the gateway endpoint from it.
E2E_GATEWAY_MODE=incluster

source "$here/lib/stack.sh"
source "$here/lib/kind.sh"
source "$here/lib/examples.sh"

E2E_COMPOSE_OVERLAYS="${E2E_COMPOSE_OVERLAYS:-} $E2E_ROOT/deploy/compose/docker-compose.kind.yml $E2E_ROOT/deploy/compose/docker-compose.incluster.yml"
EX="$E2E_ROOT/examples"
LAB_NS="regression-lab"   # fixed by examples/regression-lab/rbac.yaml
RUNS="pg16-8-clients pg17-8-clients pg18-8-clients"

kind_up
e2e_on_teardown kind_down
stack_up
# The operators before the gateway: the Cluster CRD must exist when it looks
# for the kinds it serves.
kind_cnpg
kind_metrics_server
kind_deploy_gateway "pods,jobs.batch,clusters.postgresql.cnpg.io,pods.metrics.k8s.io"

psql_axiom "CREATE EXTENSION IF NOT EXISTS axiom;"
psql_axiom "DROP SERVER IF EXISTS kind CASCADE;"
psql_axiom "CREATE SERVER kind FOREIGN DATA WRAPPER axiom_fdw OPTIONS (endpoint '$E2E_GATEWAY_ENDPOINT', ca_cert '/certs/ca.crt', rpc_timeout_secs '10');"

log "regression-lab: three CloudNativePG clusters, Postgres 16, 17 and 18, created from SQL"
kind_apply "$EX/regression-lab/rbac.yaml"
lab_down() {
  [[ -n "${SAMPLER_PID:-}" ]] && kill "$SAMPLER_PID" 2>/dev/null || true
  kubectl_e2e delete namespace "$LAB_NS" --ignore-not-found --wait=false >/dev/null 2>&1 || true
}
e2e_on_teardown lab_down
psql_axiom "DROP SCHEMA IF EXISTS lab CASCADE;"
psql_file "$EX/regression-lab/setup.sql" >/dev/null
psql_file "$EX/regression-lab/lab-example.sql" >/dev/null
# The operator's admission webhook can refuse a Cluster for a few seconds
# after the operator reports ready. Retry that, and only that.
deadline=$((SECONDS + 90))
until out="$(psql_file "$EX/regression-lab/clusters.sql" -v namespace="$LAB_NS" 2>&1)"; do
  grep -qi "webhook" <<<"$out" || fail "clusters.sql failed: $out"
  (( SECONDS < deadline )) || fail "the CloudNativePG webhook kept refusing clusters.sql: $out"
  sleep 3
done
deadline=$((SECONDS + 480))
until [[ "$(psql_axiom "SELECT count(*) FROM lab.pg_clusters
                         WHERE namespace = '$LAB_NS' AND status->>'phase' = 'Cluster in healthy state';")" == 3 ]]; do
  (( SECONDS < deadline )) || fail "the clusters did not become healthy: $(psql_axiom "SELECT name, status->>'phase' FROM lab.pg_clusters WHERE namespace = '$LAB_NS';")"
  sleep 5
done
echo "pg16, pg17 and pg18 are healthy"

log "regression-lab: pgbench against each, sampled while it runs"
# Sampled from the host every five seconds for as long as the runs go, as a
# reader would with \watch; a failed sample is retried by the next one.
( while :; do psql_file "$EX/regression-lab/sample.sql" -v namespace="$LAB_NS" >/dev/null 2>&1 || true; sleep 5; done ) &
SAMPLER_PID=$!
psql_file "$EX/regression-lab/launch.sql" -v namespace="$LAB_NS" >/dev/null
jobs="$(kubectl_e2e -n "$LAB_NS" get jobs -o jsonpath='{.items[*].metadata.name}')"
[[ "$(tr ' ' '\n' <<<"$jobs" | sort | paste -sd, -)" == "bench-missing-cluster,bench-pg16-8-clients,bench-pg17-8-clients,bench-pg18-8-clients" ]] \
  || fail "launch.sql created jobs '$jobs'"
psql_file "$EX/regression-lab/launch.sql" -v namespace="$LAB_NS" >/dev/null
[[ "$(kubectl_e2e -n "$LAB_NS" get jobs --no-headers | wc -l | tr -d ' ')" == 4 ]] \
  || fail "a second launch.sql started more Jobs"

# results.sql's columns, as the reads below take them:
#   1 run  2 cluster  3 server_version  4 cpu_limit  5 clients  6 state  7 tps
#   8 latency_ms  9 pg_cpu_avg  10 tps_per_core  11 pg_cpu_peak
#   12 pg_memory_peak  13 samples  14 pgbench_cpu_avg  15 error
results() { psql_file "$EX/regression-lab/results.sql" -v namespace="$LAB_NS"; }
state_of() { results | awk -F'|' -v r="$1" '$1 == r { print $6 }'; }
# Wait for the real runs only: the run against a cluster that does not exist
# waits for its Secret forever, which is what it is there to show.
deadline=$((SECONDS + 480))
for run in $RUNS; do
  until [[ "$(state_of "$run")" =~ ^(done|failed)$ ]]; do
    (( SECONDS < deadline )) || fail "the runs did not finish: $(results)"
    sleep 5
  done
done
# One more sample, so the last window of each run is in.
sleep 20
psql_file "$EX/regression-lab/sample.sql" -v namespace="$LAB_NS" >/dev/null 2>&1 || true
kill "$SAMPLER_PID" 2>/dev/null || true; SAMPLER_PID=""
table="$(psql_table "$EX/regression-lab/results.sql" -v namespace="$LAB_NS")"
echo "$table"
[[ -n "${E2E_LAB_REPORT:-}" ]] && printf '%s\n' "$table" > "$E2E_LAB_REPORT"

got="$(results)"
for run in $RUNS; do
  IFS='|' read -r _ _ version _ _ state tps latency _ _ peak _ samples _ error <<<"$(grep "^$run|" <<<"$got")"
  [[ "$state" == done ]] || fail "$run is '$state': $error"
  [[ "$(psql_axiom "SELECT '$tps'::numeric > 0 AND '$latency'::numeric > 0;")" == t ]] \
    || fail "$run reported tps '$tps' and latency '$latency'"
  # The major under test is the one the run measured.
  [[ "$version" == "${run:2:2}".* ]] || fail "$run measured server version '$version'"
  # The claim the example makes: resource use of the Postgres under test,
  # from inside the benchmark's window. At least two samples from there.
  [[ "${samples:-0}" -ge 2 ]] || fail "$run has ${samples:-0} Postgres samples inside its benchmark window"
  # And that they are the right Pods: no cluster can use more CPU than its
  # 500m limit, give or take the averaging window.
  [[ "$(psql_axiom "SELECT '$peak'::numeric <= 0.5 * 1.25;")" == t ]] \
    || fail "$run's Postgres peaked at $peak cores with a 500m limit: those samples are not its Pods"
done
IFS='|' read -r _ _ _ _ _ state _ _ _ _ _ _ _ _ error <<<"$(grep "^missing-cluster|" <<<"$got")"
[[ "$state" == waiting ]] && grep -q "pg-missing-app" <<<"$error" \
  || fail "the run against a missing cluster is '$state' with '$error', want waiting on its Secret"
echo "three majors benchmarked with in-window Postgres CPU; the missing cluster's run waits on its Secret"

log "LAB E2E PASSED"
