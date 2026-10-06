#!/usr/bin/env bash
# Regression-lab E2E (#104, #107): examples/regression-lab run as shipped
# against a kind cluster with CloudNativePG and metrics-server. Three
# CloudNativePG clusters, Postgres 16, 17 and 18, are created from SQL; a
# pgbench Job runs against each, one at a time; their resource use is sampled
# while the benchmark runs; and the lab.results view (results.sql) joins the two.
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
# metrics-server reports a new Pod only after it has scraped it; a benchmark
# started before then loses its first windows. Start the queue once every
# cluster's Postgres has been sampled at least once.
deadline=$((SECONDS + 180))
until [[ "$(psql_axiom "SELECT count(DISTINCT cluster) FROM lab.usage WHERE namespace = '$LAB_NS' AND role = 'postgres';")" == 3 ]]; do
  (( SECONDS < deadline )) || fail "metrics-server never reported all three clusters: $(psql_axiom "SELECT cluster, count(*) FROM lab.usage GROUP BY 1;")"
  sleep 5
done
# lab.results's columns, as the reads below take them:
#   1 run  2 cluster  3 server_version  4 cpu_limit  5 clients  6 state  7 tps
#   8 latency_ms  9 pg_cpu_avg  10 tps_per_core  11 pg_cpu_peak
#   12 pg_memory_peak  13 samples  14 pgbench_cpu_avg  15 error
results() { psql_file "$EX/regression-lab/results.sql"; }
unfinished() {
  kubectl_e2e -n "$LAB_NS" get jobs -o jsonpath='{range .items[*]}{.status.succeeded}{.status.failed}{"\n"}{end}' \
    | grep -c '^$' || true
}
# Drive the queue as a reader would with \watch: launch.sql starts the next
# run only when none is unfinished, so calling it on a loop runs them in turn.
# At no point may two benchmarks run at once.
deadline=$((SECONDS + 600))
while :; do
  psql_file "$EX/regression-lab/launch.sql" -v namespace="$LAB_NS" >/dev/null
  running="$(unfinished)"
  (( running <= 1 )) || fail "$running benchmark Jobs are unfinished at once; launch.sql must run them one at a time"
  done_runs="$(results | awk -F'|' '$6 == "done" || $6 == "failed"' | wc -l | tr -d ' ')"
  [[ "$done_runs" == 3 ]] && break
  (( SECONDS < deadline )) || fail "the runs did not finish: $(results)"
  sleep 5
done
jobs="$(kubectl_e2e -n "$LAB_NS" get jobs -o jsonpath='{.items[*].metadata.name}')"
[[ "$(tr ' ' '\n' <<<"$jobs" | sort | paste -sd, -)" == "bench-pg16-8-clients,bench-pg17-8-clients,bench-pg18-8-clients" ]] \
  || fail "launch.sql created jobs '$jobs'"
# Every run has its Job, so launching again starts nothing.
psql_file "$EX/regression-lab/launch.sql" -v namespace="$LAB_NS" >/dev/null
[[ "$(kubectl_e2e -n "$LAB_NS" get jobs --no-headers | wc -l | tr -d ' ')" == 3 ]] \
  || fail "a launch.sql after every run finished started another Job"
# One more sample, so the last window of each run is in.
sleep 20
psql_file "$EX/regression-lab/sample.sql" -v namespace="$LAB_NS" >/dev/null 2>&1 || true
kill "$SAMPLER_PID" 2>/dev/null || true; SAMPLER_PID=""
# What both READMEs quote: `SELECT * FROM lab.results;`, which is results.sql.
echo "=== SELECT * FROM lab.results;"
table="$(psql_table "$EX/regression-lab/results.sql")"
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
  [[ "${samples:-0}" -ge 2 ]] || fail "$run has ${samples:-0} Postgres samples inside its benchmark window.
Its result: $(grep "^$run|" <<<"$got")
Every sample of its cluster:
$(psql_axiom "SELECT u.pod, u.sampled_at, u.window_s, round(u.cpu_cores, 2) FROM lab.usage u JOIN lab.runs r ON r.cluster = u.cluster WHERE r.run = '$run' ORDER BY u.sampled_at;")"
  # And that they are the right Pods: no cluster can use more CPU than its
  # 2-CPU limit, give or take the averaging window.
  [[ "$(psql_axiom "SELECT '$peak'::numeric <= 2 * 1.25;")" == t ]] \
    || fail "$run's Postgres peaked at $peak cores with a 2-CPU limit: those samples are not its Pods"
done
echo "three majors benchmarked one at a time, each with in-window Postgres CPU"

log "LAB E2E PASSED"
