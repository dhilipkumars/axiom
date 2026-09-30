#!/usr/bin/env bash
# Examples E2E (#104): two of the programs under examples/ run as shipped
# against a kind cluster. Every file the READMEs tell a reader to run is the
# file run here, so the examples cannot drift from what works.
#
#   sql-operator     an operator whose reconcile step is one UPDATE, woken by
#                    NOTIFY axiom_events and kept correct by a periodic sweep
#   deploy-timeline  a Helm release's Deployment, ReplicaSet, Pod and events
#                    on one timeline, from one query
#
# The third, the regression lab, installs CloudNativePG and metrics-server and
# runs minutes of benchmarks, so it is its own gate, e2e/lab_test.sh, run on
# demand rather than on every pull request (#107).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Must precede the source: stack.sh resolves the gateway endpoint from it.
E2E_GATEWAY_MODE=incluster

source "$here/lib/stack.sh"
source "$here/lib/kind.sh"

E2E_COMPOSE_OVERLAYS="${E2E_COMPOSE_OVERLAYS:-} $E2E_ROOT/deploy/compose/docker-compose.kind.yml $E2E_ROOT/deploy/compose/docker-compose.incluster.yml"
EX="$E2E_ROOT/examples"
OP_NS="axiom-sqlop"
TL_NS="axiom-timeline"

kind_up
e2e_on_teardown kind_down
stack_up
kind_deploy_gateway "configmaps,pods,events,services,deployments.apps,replicasets.apps"

source "$here/lib/examples.sh"

psql_axiom "CREATE EXTENSION IF NOT EXISTS axiom;"
psql_axiom "DROP SERVER IF EXISTS kind CASCADE;"
psql_axiom "CREATE SERVER kind FOREIGN DATA WRAPPER axiom_fdw OPTIONS (endpoint '$E2E_GATEWAY_ENDPOINT', ca_cert '/certs/ca.crt', rpc_timeout_secs '10');"

# --- sql-operator ---------------------------------------------------------------------

log "sql-operator: setup.sql, and a namespace the owners table assigns to a team"
psql_axiom "DROP SCHEMA IF EXISTS sqlop CASCADE;"
psql_file "$EX/sql-operator/setup.sql" >/dev/null
kubectl_e2e create namespace "$OP_NS" --dry-run=client -o yaml | kubectl_e2e apply -f - >/dev/null
kubectl_e2e -n "$OP_NS" delete configmap --all --ignore-not-found >/dev/null
psql_axiom "INSERT INTO sqlop.owners VALUES ('$OP_NS', 'payments');"
# Cleared first: docker cp into an existing directory nests the copy inside it.
compose exec -T "$E2E_SVC_POSTGRES" rm -rf /tmp/sql-operator >/dev/null
compose cp "$EX/sql-operator" "$E2E_SVC_POSTGRES:/tmp/sql-operator" >/dev/null || fail "copy the operator into the Postgres container"

# op_log: what the operator has logged, read from inside the container. Its
# output is written to a file there rather than streamed back through a
# backgrounded `compose exec`, which delivered the first line and then none of
# the rest -- the gateway's own log showed the reconciles happening.
op_log() { compose exec -T "$E2E_SVC_POSTGRES" cat /tmp/sql-operator.log 2>/dev/null || true; }
# operator_start SWEEP_SECONDS: run operator.sh detached in the Postgres
# container, in its own session so operator_stop can end it and its psql
# children together.
operator_start() {
  compose exec -d -e PGUSER="$E2E_PG_USER" -e PGDATABASE="$E2E_PG_DB" -e SWEEP_SECONDS="$1" \
    "$E2E_SVC_POSTGRES" setsid bash -c \
    'echo $$ > /tmp/sql-operator.pid; exec bash /tmp/sql-operator/operator.sh > /tmp/sql-operator.log 2>&1' \
    || fail "start the operator"
  local deadline=$((SECONDS + 60))
  until op_log | grep -q "reconciled (startup)"; do
    (( SECONDS < deadline )) || fail "the operator did not complete its startup reconcile: $(op_log)"
    sleep 1
  done
}
operator_stop() {
  compose exec -T "$E2E_SVC_POSTGRES" bash -c \
    '[[ -f /tmp/sql-operator.pid ]] && kill -TERM -- -"$(cat /tmp/sql-operator.pid)" 2>/dev/null; rm -f /tmp/sql-operator.pid' \
    >/dev/null 2>&1 || true
}
e2e_on_teardown operator_stop
# logged PATTERN WHAT: wait for the operator to log PATTERN. The label can
# reach the cluster a moment before the reconcile that wrote it returns and
# logs, so this waits rather than reading the log once.
logged() {
  local deadline=$((SECONDS + 10))
  until op_log | grep -q "$1"; do
    (( SECONDS < deadline )) || fail "$2: $(op_log)"
    sleep 1
  done
}
# label_is CM WANT SECS WHAT: wait for a ConfigMap's team label.
label_is() {
  local cm="$1" want="$2" deadline=$((SECONDS + $3)) got=""
  while (( SECONDS < deadline )); do
    got="$(kubectl_e2e -n "$OP_NS" get configmap "$cm" -o jsonpath='{.metadata.labels.team}' 2>/dev/null || true)"
    [[ "$got" == "$want" ]] && return 0
    sleep 1
  done
  fail "$4: configmap $cm has team='$got' after $3s, want '$want'. Operator log:
$(op_log)"
}

# The first two checks run with a sweep far longer than their deadline, so the
# label can only arrive because a notification woke the loop. A broken NOTIFY
# path would otherwise pass on the timer alone.
operator_start 600
deadline=$((SECONDS + 90))
until [[ "$(psql_axiom "SELECT state FROM axiom_watch_status() WHERE resource = 'configmaps' LIMIT 1;" 2>/dev/null || true)" == ACTIVE ]]; do
  (( SECONDS < deadline )) || fail "the operator's watch on configmaps never became ACTIVE"
  sleep 1
done

log "sql-operator: a new ConfigMap is labelled within seconds, through NOTIFY"
kubectl_e2e -n "$OP_NS" create configmap op-a --from-literal=k=v >/dev/null
label_is op-a payments 20 "NOTIFY path"
logged "reconciled (notified)" "the label arrived, but not through a notified reconcile"

log "sql-operator: a label removed by hand comes back"
kubectl_e2e -n "$OP_NS" label configmap op-a team- >/dev/null
label_is op-a payments 20 "drift repair"
op_log
operator_stop

log "sql-operator: a change to the owners table reaches the cluster on the next sweep"
# Nothing in the cluster changes here, so no notification can fire: only the
# sweep can carry the new owner out.
operator_start 3
psql_axiom "UPDATE sqlop.owners SET team = 'platform' WHERE namespace = '$OP_NS';"
label_is op-a platform 20 "sweep path"
logged "reconciled (sweep)" "the relabel did not come from a sweep"
op_log
operator_stop
kubectl_e2e -n "$OP_NS" get configmaps -L team
echo "notified, drift-repaired and swept: $(op_log | grep -c reconciled) reconciles in the last run"

# --- deploy-timeline ------------------------------------------------------------------

log "deploy-timeline: install the demo chart, then read its timeline"
kubectl_e2e create namespace "$TL_NS" --dry-run=client -o yaml | kubectl_e2e apply -f - >/dev/null
helm_e2e uninstall demo -n "$TL_NS" >/dev/null 2>&1 || true
timeline_down() { helm_e2e uninstall demo -n "$TL_NS" >/dev/null 2>&1 || true; }
e2e_on_teardown timeline_down
helm_e2e install demo examples/deploy-timeline/chart -n "$TL_NS" >/dev/null || fail "helm install the demo chart"
kubectl_e2e -n "$TL_NS" rollout status deployment/demo --timeout=120s >/dev/null || fail "the demo Deployment did not roll out"

psql_axiom "DROP SCHEMA IF EXISTS k8s CASCADE; CREATE SCHEMA k8s;
            IMPORT FOREIGN SCHEMA k8s LIMIT TO (apps_deployments, apps_replicasets, core_pods, core_services, core_events)
              FROM SERVER kind INTO k8s;"
# Events arrive a moment after the rollout completes; wait for the last one.
deadline=$((SECONDS + 60))
while :; do
  timeline="$(psql_file "$EX/deploy-timeline/timeline.sql" -v namespace="$TL_NS" -v release=demo)"
  grep -q '|Pod|[^|]*|Started|' <<<"$timeline" && break
  (( SECONDS < deadline )) || fail "the timeline never showed the Pod starting:
$timeline"
  sleep 2
done
psql_table "$EX/deploy-timeline/timeline.sql" -v namespace="$TL_NS" -v release=demo
# at_of KIND STEP: the first time that kind reached that step.
at_of() { awk -F'|' -v k="$1" -v s="$2" '$2 == k && $4 == s { print $1; exit }' <<<"$timeline"; }
prev=""
for step in "Deployment created" "ReplicaSet created" "Pod created" "Pod PodScheduled" "Pod Started" "Pod Ready"; do
  at="$(at_of ${step% *} ${step#* })"
  [[ -n "$at" ]] || fail "the timeline has no '$step' row"
  # Whole seconds, so steps tie; they must never go backwards.
  if [[ -n "$prev" ]] && [[ "$(psql_axiom "SELECT '$at'::timestamptz < '$prev'::timestamptz;")" == t ]]; then
    fail "'$step' at $at comes before the step preceding it at $prev"
  fi
  prev="$at"
done
echo "Deployment, ReplicaSet and Pod created, scheduled, started and ready, in order"

log "EXAMPLES E2E PASSED"
