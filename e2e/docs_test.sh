#!/usr/bin/env bash
# Docs E2E: every query the examples pages show runs, against a cluster with
# something worth showing, and prints the table the page quotes.
#
# The pages include the query files under docs/snippets/examples/ rather than
# a copy, so what a reader pastes is what ran here. The output on the pages is
# pasted from this gate's log. It is not compared back: ages, pod-name hashes
# and memory figures differ on every run, so a byte comparison would fail on
# noise. The assertions below check what each example claims instead -- that
# the crash-looping pod is found, that the stuck rollout says why.
#
# The fixtures are e2e/fixtures/shop.yaml: a namespace with one of each
# problem the pages show how to find.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
E2E_GATEWAY_MODE=incluster

source "$here/lib/stack.sh"
source "$here/lib/kind.sh"
source "$here/lib/examples.sh"

E2E_COMPOSE_OVERLAYS="${E2E_COMPOSE_OVERLAYS:-} $E2E_ROOT/deploy/compose/docker-compose.kind.yml $E2E_ROOT/deploy/compose/docker-compose.incluster.yml"
Q="$E2E_ROOT/docs/snippets/examples"

kind_up
e2e_on_teardown kind_down
kind_metrics_server
stack_up
kind_deploy_gateway "pods,configmaps,events,events.events.k8s.io,pods.metrics.k8s.io,deployments.apps,replicasets.apps"

log "applying the shop the examples look at"
# A shop left by an earlier run on a kept cluster is still terminating, and
# applying into a terminating namespace fails.
kubectl_e2e delete -f "$here/fixtures/shop.yaml" --ignore-not-found --wait=true >/dev/null 2>&1 || true
kind_apply "$here/fixtures/shop.yaml"
shop_down() {
  kubectl_e2e delete -f "$here/fixtures/shop.yaml" --ignore-not-found --wait=false >/dev/null 2>&1 || true
}
e2e_on_teardown shop_down
for d in web catalog; do
  kubectl_e2e -n shop rollout status "deploy/$d" --timeout=120s >/dev/null || fail "deployment $d did not roll out"
done

log "server prod and schema k8s, as Getting started leaves them"
psql_axiom "CREATE EXTENSION IF NOT EXISTS axiom;"
psql_axiom "DROP SERVER IF EXISTS prod CASCADE;"
psql_axiom "CREATE SERVER prod FOREIGN DATA WRAPPER axiom_fdw OPTIONS (endpoint '$E2E_GATEWAY_ENDPOINT', ca_cert '/certs/ca.crt', rpc_timeout_secs '15');"
psql_axiom "DROP SCHEMA IF EXISTS k8s CASCADE; CREATE SCHEMA k8s;"
psql_axiom "IMPORT FOREIGN SCHEMA k8s FROM SERVER prod INTO k8s;"
psql_axiom "DROP TABLE IF EXISTS tenants;"

# until_shows FILE PATTERN WHAT: rerun a query until its output matches. The
# problems take a little while to become visible: a crash loop needs a couple
# of restarts, and metrics-server reports a pod only after a scrape.
until_shows() {
  local file="$Q/$1" pattern="$2" what="$3" deadline=$((SECONDS + 240)) out
  while :; do
    out="$(psql_file "$file" 2>&1 || true)"
    grep -qE "$pattern" <<<"$out" && return 0
    (( SECONDS < deadline )) || fail "$what did not appear within 240s; last output: $out"
    sleep 5
  done
}

# show FILE: print a query file and its result as the page quotes it.
show() {
  echo
  echo "=== docs/snippets/examples/$1"
  psql_table "$Q/$1" || fail "$1 failed"
}

log "waiting for each problem to become visible"
until_shows crash-looping.sql '^shop\|checkout-worker\|Running\|worker\|([3-9]|[1-9][0-9]+)\|' "the crash-looping pod, three restarts in"
until_shows not-running.sql '^shop\|report\|' "the warning on the pod whose image does not exist"
until_shows not-running.sql '^shop\|checkout-' "the warning on the pod that cannot be scheduled"
# The pods the usage examples read, by name: a count could be met by pods the
# examples do not depend on, and find-and-fix needs catalog's sample.
deadline=$((SECONDS + 240))
until [[ "$(psql_axiom "SELECT count(DISTINCT split_part(name, '-', 1)) FROM k8s.metrics_k8s_io_pods
                         WHERE namespace = 'shop' AND split_part(name, '-', 1) IN ('web', 'catalog');")" == 2 ]]; do
  (( SECONDS < deadline )) || fail "metrics-server reported no usage for the shop pods within 240s"
  sleep 5
done

log "the landing page"
show crash-looping.sql
show not-running.sql
show stuck-rollouts.sql
psql_file "$Q/stuck-rollouts.sql" | grep -q '^shop|checkout|0/1|checkout-.*Insufficient cpu' \
  || fail "the stuck rollout does not say it is short of CPU"

log "query patterns, in the Querying guide"
show aggregate.sql
show filter-rollouts.sql
show filter-raw.sql
psql_file "$Q/filter-raw.sql" | grep -q '^shop|web|web$' || fail "web sets no memory limit, and was not found"
psql_file "$Q/filter-raw.sql" | grep -q '^shop|catalog|' && fail "catalog sets a memory limit, and was listed"

log "capacity and risk review"
show quantities.sql
show fleet-review.sql
psql_file "$Q/fleet-review.sql" | grep -q '^shop|catalog|1|' || fail "the fleet review has no row for catalog"

log "changing the cluster"
show write-configmap.sql
[[ "$(kubectl_e2e -n shop get configmap checkout-config -o jsonpath='{.data.LOG_LEVEL}')" == debug ]] \
  || fail "the ConfigMap UPDATE did not reach the cluster"
echo
echo "=== docs/snippets/examples/pods-read-only.sql"
out="$(psql_table "$Q/pods-read-only.sql" 2>&1 || true)"
echo "$out"
grep -q "does not allow deletes" <<<"$out" || fail "deleting a pod was not refused: $out"
kubectl_e2e -n shop get pod checkout-worker >/dev/null || fail "the refused DELETE removed the pod anyway"
show find-and-fix.sql
echo
echo "=== kubectl -n shop get deploy catalog -o jsonpath='{.metadata.annotations.axiom/memory-used-pct}'"
kubectl_e2e -n shop get deploy catalog -o jsonpath='{.metadata.annotations.axiom/memory-used-pct}'
echo
[[ -n "$(kubectl_e2e -n shop get deploy catalog -o jsonpath='{.metadata.annotations.axiom/memory-used-pct}')" ]] \
  || fail "find-and-fix did not annotate catalog, which uses a fraction of its 256Mi"

log "your own data"
psql_file "$Q/tenants-setup.sql" >/dev/null || fail "tenants-setup.sql failed"
show tenants-failing.sql
psql_file "$Q/tenants-failing.sql" | grep -q '^Acme Corp|enterprise|report|' \
  || fail "the customer join does not show the failing report pod"
show tenants-memory.sql

log "DOCS E2E PASSED"
