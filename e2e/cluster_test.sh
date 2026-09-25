#!/usr/bin/env bash
# Whole-cluster data model E2E (Phase 5 in docs/PLAN.md): one schema named for
# the cluster, one table per kind the gateway's RBAC actually permits.
#
# The gateway serves "*.*" here, so every assertion about which tables exist is
# a consequence of e2e/fixtures/phase5-rbac.yaml alone. That is the point: the
# ServiceAccount is the boundary, not a second list kept in step by hand.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Must precede the source: stack.sh resolves the gateway endpoint from it.
E2E_GATEWAY_MODE=incluster

source "$here/lib/stack.sh"
source "$here/lib/kind.sh"

E2E_COMPOSE_OVERLAYS="${E2E_COMPOSE_OVERLAYS:-} $E2E_ROOT/deploy/compose/docker-compose.kind.yml $E2E_ROOT/deploy/compose/docker-compose.incluster.yml"
NS="axiom-e2e"
SA="system:serviceaccount:$E2E_GATEWAY_SA_NS:$E2E_GATEWAY_SA"
# The schema is named for the cluster, which is the Phase 5 convention.
SCHEMA="axiom_e2e"

kind_up
e2e_on_teardown kind_down

log "applying the test CRD and a deliberately partial RBAC set"
kubectl_e2e create namespace "$NS" --dry-run=client -o yaml | kubectl_e2e apply -f - >/dev/null
kubectl_e2e apply -f "$E2E_ROOT/e2e/fixtures/widget-crd.yaml" >/dev/null || fail "apply CRD"
kubectl_e2e wait --for=condition=Established crd/widgets.example.com --timeout=60s >/dev/null \
  || fail "CRD did not become Established"
kubectl_e2e -n "$NS" delete widgets --all --ignore-not-found >/dev/null
kubectl_e2e apply -f "$E2E_ROOT/e2e/fixtures/widgets.yaml" >/dev/null || fail "apply widgets"
kubectl_e2e apply -f "$E2E_ROOT/e2e/fixtures/phase5-rbac.yaml" >/dev/null || fail "apply phase5 RBAC"
e2e_on_teardown phase5_rbac_down
phase5_rbac_down() {
  kubectl_e2e delete -f "$E2E_ROOT/e2e/fixtures/phase5-rbac.yaml" --ignore-not-found >/dev/null 2>&1 || true
}

stack_up
# "*.*" so RBAC is the only bound on what is offered -- the whole point of this gate.
# And the fixture as the only *read* grant, so the exact table set asserted below
# is a consequence of it rather than of Kubernetes' `view` role, which the
# shipped RBAC aggregates.
E2E_UNBIND_SHIPPED_READ=1 kind_deploy_gateway "*.*"
e2e_on_teardown shipped_read_restore
shipped_read_restore() {
  kubectl_e2e apply -f "$E2E_ROOT/deploy/k8s/gateway-rbac.yaml" >/dev/null 2>&1 || true
}

log "the gateway reports RBAC as its only bound"
stack_logs "$E2E_SVC_GATEWAY" | grep -q '"bounded_by":"rbac"' \
  || fail "gateway did not log rbac as the bound; --serve is narrowing too"

# --- one schema for the cluster, one table per permitted kind -----------------------

log "IMPORT the whole cluster into a schema named for it"
psql_axiom "CREATE EXTENSION IF NOT EXISTS axiom;"
psql_axiom "DROP SERVER IF EXISTS $SCHEMA CASCADE;"
psql_axiom "DROP SCHEMA IF EXISTS $SCHEMA CASCADE;"
psql_axiom "CREATE SERVER $SCHEMA FOREIGN DATA WRAPPER axiom_fdw OPTIONS (endpoint '$E2E_GATEWAY_ENDPOINT', ca_cert '/certs/ca.crt', rpc_timeout_secs '30');"
psql_axiom "CREATE SCHEMA $SCHEMA;"
psql_axiom "IMPORT FOREIGN SCHEMA $SCHEMA FROM SERVER $SCHEMA INTO $SCHEMA;"

tables() { psql_axiom "SELECT string_agg(c.relname, ',' ORDER BY c.relname) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = '$SCHEMA' AND c.relkind = 'f';"; }
got="$(tables)"
echo "tables: $got"

can_i() { kubectl_e2e --as="$SA" auth can-i "$1" "$2" -n "$NS" 2>/dev/null || true; }
can_i_cluster() { kubectl_e2e --as="$SA" auth can-i "$1" "$2" 2>/dev/null || true; }
# Listable kinds in the apps group, as table names (`apps_<plural>`). The
# fixture grants apps/*, and which kinds that covers depends on the cluster
# version.
apps_kinds() {
  kubectl_e2e api-resources --api-group=apps --verbs=list -o name 2>/dev/null |
    sed 's/\..*//' | sort -u | sed 's/^/apps_/'
}
# has_table NAME: whether the comma-separated $got holds exactly that table.
has_table() { [[ ",$got," == *",$1,"* ]]; }

log "every kind RBAC permits became a table, and nothing else did"
# Granted explicitly in the fixture.
for want in core_configmaps core_pods example_com_widgets core_events events_k8s_io_events; do
  has_table "$want" || fail "expected a table for '$want', got: $got"
done
# The fixture grants apps/* , so every listable kind in that group must appear.
# Derived rather than listed: which kinds `apps` holds varies by cluster version,
# and hardcoding one version's set is what broke this gate in CI before.
for want in $(apps_kinds); do
  has_table "$want" || fail "apps/* is granted but '$want' was not offered: $got"
done
# Present in the cluster, allowed by --serve, but NOT granted to the identity.
for unwanted in core_secrets core_nodes core_namespaces core_serviceaccounts core_persistentvolumes; do
  has_table "$unwanted" && fail "'$unwanted' is not granted by RBAC but was offered: $got"
done
# The set is exactly what the identity may list. That is the five kinds this
# fixture grants, plus whatever Kubernetes grants every ServiceAccount through
# its own default bindings -- `clustertrustbundles` is bound to the
# `system:serviceaccounts` group, for instance. Which of those exist depends on
# the cluster version, so derive the expectation instead of hardcoding it: the
# property under test is "the tables are exactly the listable kinds", not any
# particular list. Following RBAC honestly means offering the extras, and that
# is precisely why --serve survives as optional narrowing.
want="core_configmaps,core_events,events_k8s_io_events,core_pods,example_com_widgets"
for k in $(apps_kinds); do want="$want,$k"; done
for extra in clustertrustbundles.certificates.k8s.io; do
  if [[ "$(can_i_cluster list "$extra")" == "yes" ]]; then
    want="$want,certificates_k8s_io_${extra%%.*}"
    log "note: $extra is granted by a built-in binding, not by this fixture, so it is offered"
  fi
done
want="$(tr ',' '\n' <<<"$want" | sort | paste -sd, -)"
[[ "$got" == "$want" ]] || fail "table set is
  '$got'
  want '$want'"

log "the gateway's own identity confirms the boundary"
[[ "$(can_i list pods)" == "yes" ]] || fail "SA cannot list pods"
[[ "$(can_i list deployments.apps)" == "yes" ]] || fail "SA cannot list deployments (apps/* is granted)"
# Withheld, and therefore absent from the import above.
for denied in secrets nodes namespaces serviceaccounts; do
  [[ "$(can_i list "$denied")" == "no" ]] || fail "SA can list $denied but the fixture does not grant it"
done

# --- the events collision -----------------------------------------------------------

log "the two events APIs are two distinct, queryable tables, each named for its group"
core_kind="$(psql_axiom "SELECT DISTINCT kind FROM $SCHEMA.core_events LIMIT 1;")"
new_kind="$(psql_axiom "SELECT DISTINCT kind FROM $SCHEMA.events_k8s_io_events LIMIT 1;")"
[[ "$core_kind" == "Event" ]] || fail "core_events kind column is '$core_kind'"
[[ "$new_kind" == "Event" ]] || fail "events_k8s_io_events kind column is '$new_kind'"
core_api="$(psql_axiom "SELECT DISTINCT api_version FROM $SCHEMA.core_events LIMIT 1;")"
new_api="$(psql_axiom "SELECT DISTINCT api_version FROM $SCHEMA.events_k8s_io_events LIMIT 1;")"
[[ "$core_api" == "v1" ]] || fail "core_events api_version is '$core_api', want v1"
[[ "$new_api" == "events.k8s.io/v1" ]] || fail "events_k8s_io_events api_version is '$new_api'"
echo "core=$core_api  new=$new_api"
# No table carries a bare plural: every name is qualified by its group.
has_table events && fail "a bare 'events' table exists: $got"

# --- the universal columns ----------------------------------------------------------

log "api_version, kind and metadata are populated on a built-in and on a CRD"
got="$(psql_axiom "SELECT api_version || '|' || kind || '|' || (metadata->>'name') FROM $SCHEMA.core_pods WHERE namespace = 'kube-system' ORDER BY name LIMIT 1;")"
[[ "$got" == v1\|Pod\|* ]] || fail "pods universal columns: '$got'"
echo "pod:    $got"
got="$(psql_axiom "SELECT api_version || '|' || kind || '|' || (metadata->>'name') FROM $SCHEMA.example_com_widgets WHERE namespace = '$NS' AND name = 'sprocket';")"
[[ "$got" == "example.com/v1|Widget|sprocket" ]] || fail "widget universal columns: '$got'"
echo "widget: $got"

log "metadata carries fields no individual column promotes"
got="$(psql_axiom "SELECT metadata ? 'uid' AND metadata ? 'creationTimestamp' FROM $SCHEMA.example_com_widgets WHERE name = 'sprocket';")"
[[ "$got" == "t" ]] || fail "metadata is missing fields it should carry: '$got'"

log "the universal columns are the basis for a query spanning kinds"
got="$(psql_axiom "SELECT string_agg(kind, ',' ORDER BY kind) FROM (
  (SELECT kind FROM $SCHEMA.core_pods WHERE namespace = 'kube-system' LIMIT 1)
  UNION ALL (SELECT kind FROM $SCHEMA.example_com_widgets WHERE namespace = '$NS' LIMIT 1)
  UNION ALL (SELECT kind FROM $SCHEMA.core_configmaps WHERE namespace = 'kube-system' LIMIT 1)) u;")"
[[ "$got" == "ConfigMap,Pod,Widget" ]] || fail "cross-kind union gave '$got'"
echo "union over three kinds: $got"

log "server-managed universal columns are refused on write"
got="$(psql_axiom "DO \$\$ BEGIN UPDATE $SCHEMA.example_com_widgets SET kind = 'Forged' WHERE name = 'sprocket'; RAISE EXCEPTION 'unexpected success';
  EXCEPTION WHEN feature_not_supported THEN RAISE NOTICE 'caught %', SQLSTATE; END \$\$;" 2>&1 || true)"
grep -q "caught 0A000" <<<"$got" || fail "writing kind should raise 0A000, got: $got"

# --- schema documents are fetched once per group-version ----------------------------

log "the import fetched one OpenAPI document per group-version, not one per kind"
fetches="$(stack_logs "$E2E_SVC_GATEWAY" | grep -c '"msg":"openapi_fetch"' || true)"
gvs="$(stack_logs "$E2E_SVC_GATEWAY" | grep '"msg":"openapi_fetch"' | grep -o '"group_version":"[^"]*"' | sort -u | wc -l | tr -d ' ')"
[[ "$fetches" -gt 0 ]] || fail "no openapi_fetch lines at all; the assertion cannot be trusted"
[[ "$fetches" == "$gvs" ]] \
  || fail "fetched $fetches documents for $gvs group-versions: the per-group-version cache is not working"
echo "fetched $fetches document(s) across $gvs group-version(s)"

log "a second import refetches nothing"
psql_axiom "DROP SCHEMA IF EXISTS reimport CASCADE;" >/dev/null
psql_axiom "CREATE SCHEMA reimport;"
psql_axiom "IMPORT FOREIGN SCHEMA k8s FROM SERVER $SCHEMA INTO reimport;"
after="$(stack_logs "$E2E_SVC_GATEWAY" | grep -c '"msg":"openapi_fetch"' || true)"
[[ "$after" == "$fetches" ]] || fail "re-import refetched documents: $after != $fetches"
echo "still $after after a second import"

# --- RBAC changes are reflected on re-import ----------------------------------------

log "revoking a kind's RBAC removes it from the next import"
# Revoke events.k8s.io, not widgets: the base deploy/k8s/gateway-rbac.yaml that
# kind_up applies also grants example.com/widgets (from Phase 4), so revoking
# widgets here would leave the other ClusterRole still granting them. Rule 1 of
# this fixture is the only grant for events.k8s.io, which also makes this a
# sharper test: the two halves of the `events` collision must be gated
# independently of each other.
kubectl_e2e patch clusterrole axiom-gateway-phase5 --type=json \
  -p '[{"op":"replace","path":"/rules/1/resources","value":["nothing"]}]' >/dev/null \
  || fail "failed to revoke events.k8s.io"
# The gateway caches what it is allowed for its lifetime by design: an import
# asks about every kind at once and RBAC does not change mid-import. Restarting
# is how an operator picks up a revoked grant.
kind_restart_gateway
stack_wait_for_log "$E2E_SVC_GATEWAY" '"msg":"gateway listening"' 60 >/dev/null
psql_axiom "DROP SCHEMA IF EXISTS narrowed CASCADE;" >/dev/null
psql_axiom "CREATE SCHEMA narrowed;"
psql_axiom "IMPORT FOREIGN SCHEMA k8s FROM SERVER $SCHEMA INTO narrowed;"
got="$(psql_axiom "SELECT string_agg(c.relname, ',' ORDER BY c.relname) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = 'narrowed' AND c.relkind = 'f';")"
has_table events_k8s_io_events && fail "events.k8s.io survived an RBAC revocation: $got"
has_table core_pods || fail "revoking events.k8s.io should not have affected pods: $got"
# #80's regression test. Core events keep exactly the name they had while
# events.k8s.io sat beside them: a table's name depends on its own group and
# plural, never on what else is imported. This used to assert the opposite --
# that core events reclaimed the bare name `events` -- which is the silent
# rename #80 reported.
has_table core_events || fail "core events were renamed when events.k8s.io went away: $got"
has_table events && fail "core events reclaimed the bare name 'events': $got"
echo "after revocation: $got"

log "cleanup"
kubectl_e2e -n "$NS" delete widgets --all --ignore-not-found --wait=false >/dev/null 2>&1 || true

log "CLUSTER E2E PASSED"
