#!/usr/bin/env bash
# CRD E2E (Phase 4 in docs/PLAN.md): a kind the extension has never heard of is
# discovered, imported as a foreign table, and then read, written and watched
# through exactly the paths Phases 1-3 built for Pods and ConfigMaps.
#
# The point of this gate is genericity. Every assertion below is the Phase 1-3
# assertion re-aimed at a CRD, so a pass means the earlier phases generalised
# rather than that Widgets got their own special case.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# The gateway runs in-cluster (Phase 6); Postgres stays in compose and reaches
# it over the Service's NodePort. This must be set *before* sourcing stack.sh,
# which resolves E2E_GATEWAY_ENDPOINT from it at source time.
E2E_GATEWAY_MODE=incluster

source "$here/lib/stack.sh"
source "$here/lib/kind.sh"

E2E_COMPOSE_OVERLAYS="${E2E_COMPOSE_OVERLAYS:-} $E2E_ROOT/deploy/compose/docker-compose.kind.yml $E2E_ROOT/deploy/compose/docker-compose.incluster.yml"
NS="axiom-e2e"
SA="system:serviceaccount:$E2E_GATEWAY_SA_NS:$E2E_GATEWAY_SA"

kind_up
e2e_on_teardown kind_down

log "applying the test CRD and its instances"
kubectl_e2e create namespace "$NS" --dry-run=client -o yaml | kubectl_e2e apply -f - >/dev/null
kubectl_e2e apply -f "$E2E_ROOT/e2e/fixtures/widget-crd.yaml" >/dev/null || fail "apply CRD"
# A CRD and its instances cannot be applied together: the API server cannot
# resolve the instances' kind until the CRD is Established.
kubectl_e2e wait --for=condition=Established crd/widgets.example.com --timeout=60s >/dev/null \
  || fail "CRD did not become Established"
# Start from exactly the fixture set: E2E_KIND_KEEP may hand us a cluster that a
# previous run left objects in, and several assertions below compare whole
# result sets against kubectl.
kubectl_e2e -n "$NS" delete widgets --all --ignore-not-found >/dev/null
kubectl_e2e delete crd gizmos.example.com --ignore-not-found >/dev/null
kubectl_e2e apply -f "$E2E_ROOT/e2e/fixtures/widgets.yaml" >/dev/null || fail "apply widgets"

# The gateway is started after the CRD exists, but discovery must also cope with
# a CRD created later; that is asserted further down.
stack_up
# gizmos is deliberately absent: this gate asserts that a kind outside
# --serve is withheld even though discovery can see it.
# 3s discovery TTL: this gate proves a deleted CRD stops being offered, and the
# five-minute default cannot be waited out in a test.
kind_deploy_gateway "pods,configmaps,widgets.example.com" "3s"

log "the gateway is running on in-cluster credentials, not a kubeconfig"
gw_args="$(kubectl_e2e -n "$E2E_GATEWAY_SA_NS" get deploy/axiom-gateway -o jsonpath='{.spec.template.spec.containers[0].args}')"
grep -q 'kubeconfig' <<<"$gw_args" && fail "gateway still uses -kubeconfig: $gw_args"
kubectl_e2e -n "$E2E_GATEWAY_SA_NS" get pod -l app.kubernetes.io/name=axiom-gateway \
  -o jsonpath='{.items[0].spec.volumes[*].projected.sources[*].serviceAccountToken.path}' 2>/dev/null \
  | grep -q token || fail "gateway Pod has no projected ServiceAccount token"
echo "in-cluster credentials confirmed"

log "IMPORT FOREIGN SCHEMA discovers the CRD and writes its DDL"
psql_axiom "CREATE EXTENSION IF NOT EXISTS axiom;"
psql_axiom "DROP SERVER IF EXISTS kind CASCADE;"
psql_axiom "DROP SCHEMA IF EXISTS k8s CASCADE;"
psql_axiom "CREATE SERVER kind FOREIGN DATA WRAPPER axiom_fdw OPTIONS (endpoint '$E2E_GATEWAY_ENDPOINT', ca_cert '/certs/ca.crt', rpc_timeout_secs '10');"
psql_axiom "CREATE SCHEMA k8s;"
psql_axiom "IMPORT FOREIGN SCHEMA k8s FROM SERVER kind INTO k8s;"

got="$(psql_axiom "SELECT string_agg(c.relname, ',' ORDER BY c.relname) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = 'k8s' AND c.relkind = 'f';")"
[[ "$got" == "core_configmaps,core_pods,example_com_widgets" ]] || fail "imported tables are '$got', want 'core_configmaps,core_pods,example_com_widgets'"
echo "imported: $got"

log "generated columns match the CRD's schema (spec/status promoted, metadata scalars, raw)"
got="$(psql_axiom "SELECT string_agg(a.attname || ' ' || format_type(a.atttypid, NULL), ', ' ORDER BY a.attnum) FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = 'k8s' AND c.relname = 'example_com_widgets' AND a.attnum > 0 AND NOT a.attisdropped;")"
want="api_version text, kind text, name text, namespace text, uid text, resource_version text, creation_timestamp text, labels jsonb, annotations jsonb, metadata jsonb, spec jsonb, status jsonb, raw jsonb"
[[ "$got" == "$want" ]] || fail "widgets columns:
  got  $got
  want $want"
echo "$got"

log "the generated DDL carries the resolved identity, so scans need no discovery"
got="$(psql_axiom "SELECT array_to_string(ftoptions, ',') FROM pg_foreign_table ft JOIN pg_class c ON c.oid = ft.ftrelid JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = 'k8s' AND c.relname = 'example_com_widgets';")"
for opt in "resource=widgets" "group=example.com" "version=v1" "kind=Widget"; do
  grep -q "$opt" <<<"$got" || fail "widgets options lack $opt: $got"
done
echo "$got"

# --- Phase 1 equivalent: read and qual pushdown -------------------------------------

log "SELECT over the CRD matches kubectl"
got="$(psql_axiom "SELECT string_agg(name || '|' || coalesce(spec->>'size', '') || '|' || coalesce(spec->>'color', ''), ',' ORDER BY name) FROM k8s.example_com_widgets WHERE namespace = '$NS';")"
want="$(kubectl_e2e -n "$NS" get widgets -o jsonpath='{range .items[*]}{.metadata.name}|{.spec.size}|{.spec.color},{end}' | sed 's/,$//' | tr ',' '\n' | sort | paste -sd, -)"
[[ "$got" == "$want" ]] || fail "SQL '$got' != kubectl '$want'"
echo "$got"

log "short names are views created on request, and read the same rows"
# #80: imported names carry their group so they never change with the cluster;
# a short name is the caller's opt-in.
got="$(psql_axiom "SELECT string_agg(short_name || '|' || status, ',' ORDER BY short_name) FROM axiom_create_short_names('k8s');")"
[[ "$got" == "configmaps|created,pods|created,widgets|created" ]] \
  || fail "axiom_create_short_names gave '$got'"
via_view="$(psql_axiom "SELECT string_agg(name, ',' ORDER BY name) FROM k8s.widgets WHERE namespace = '$NS';")"
direct="$(psql_axiom "SELECT string_agg(name, ',' ORDER BY name) FROM k8s.example_com_widgets WHERE namespace = '$NS';")"
[[ -n "$direct" && "$via_view" == "$direct" ]] || fail "short name read '$via_view', table read '$direct'"
echo "k8s.widgets -> k8s.example_com_widgets: $via_view"

log "promoted metadata columns and status are readable"
got="$(psql_axiom "SELECT labels->>'tier' || '|' || (status->>'phase') || '|' || (status->>'ready') FROM k8s.example_com_widgets WHERE namespace = '$NS' AND name = 'sprocket';")"
[[ "$got" == "backend|Running|true" ]] || fail "sprocket metadata/status is '$got', want 'backend|Running|true'"
uid="$(psql_axiom "SELECT uid FROM k8s.example_com_widgets WHERE namespace = '$NS' AND name = 'sprocket';")"
[[ "$uid" == "$(kubectl_e2e -n "$NS" get widget sprocket -o jsonpath='{.metadata.uid}')" ]] || fail "uid column disagrees with kubectl"

log "namespace and name quals are pushed down to the gateway, not filtered locally"
gw_lists() { psql_axiom "SELECT list_calls FROM axiom_gateway_stats('kind');"; }
before="$(gw_lists)"
psql_axiom "SELECT count(*) FROM k8s.example_com_widgets WHERE namespace = '$NS' AND name = 'cog';" >/dev/null
stack_logs "$E2E_SVC_GATEWAY" | grep '"msg":"list"' | tail -1 | grep -q "\"name\":\"cog\"" \
  || fail "name qual was not pushed down"
stack_logs "$E2E_SVC_GATEWAY" | grep '"msg":"list"' | tail -1 | grep -q "\"namespace\":\"$NS\"" \
  || fail "namespace qual was not pushed down"
after="$(gw_lists)"
[[ "$after" -gt "$before" ]] || fail "the scan issued no List at all"

log "a nonexistent widget is an empty result, not an error"
got="$(psql_axiom "SELECT count(*) FROM k8s.example_com_widgets WHERE namespace = '$NS' AND name = 'nosuchwidget';")"
[[ "$got" == "0" ]] || fail "count for a missing widget is '$got'"

# --- Phase 2 equivalent: the write path ---------------------------------------------

log "INSERT creates the Widget in the cluster"
uid="$(psql_axiom "INSERT INTO k8s.example_com_widgets (name, namespace, spec) VALUES ('flange', '$NS', '{\"size\":11,\"color\":\"green\"}') RETURNING uid;")"
[[ -n "$uid" ]] || fail "INSERT RETURNING gave no uid"
want="$(kubectl_e2e -n "$NS" get widget flange -o jsonpath='{.metadata.uid}|{.spec.size}|{.spec.color}')"
[[ "$want" == "$uid|11|green" ]] || fail "cluster shows '$want', want '$uid|11|green'"
echo "created uid=$uid"

log "duplicate INSERT is unique_violation (23505)"
got="$(psql_axiom "DO \$\$ BEGIN INSERT INTO k8s.example_com_widgets (name, namespace, spec) VALUES ('flange', '$NS', '{}'); RAISE EXCEPTION 'unexpected success';
  EXCEPTION WHEN unique_violation THEN RAISE NOTICE 'caught %', SQLSTATE; END \$\$;" 2>&1 || true)"
grep -q "caught 23505" <<<"$got" || fail "expected 23505, got: $got"

log "UPDATE of a top-level column changes only that field"
kubectl_e2e -n "$NS" label widget flange tier=frontend --overwrite >/dev/null
psql_axiom "UPDATE k8s.example_com_widgets SET spec = spec || '{\"color\":\"orange\"}' WHERE namespace = '$NS' AND name = 'flange';"
want="$(kubectl_e2e -n "$NS" get widget flange -o jsonpath='{.spec.size}|{.spec.color}|{.metadata.labels.tier}')"
[[ "$want" == "11|orange|frontend" ]] || fail "cluster shows '$want', want '11|orange|frontend' (size and label untouched)"

log "UPDATE of labels writes metadata, not a top-level field"
psql_axiom "UPDATE k8s.example_com_widgets SET labels = labels || '{\"env\":\"test\"}' WHERE namespace = '$NS' AND name = 'flange';"
want="$(kubectl_e2e -n "$NS" get widget flange -o jsonpath='{.metadata.labels.env}|{.metadata.labels.tier}')"
[[ "$want" == "test|frontend" ]] || fail "labels are '$want', want 'test|frontend'"

log "UPDATE through raw changes the object even when the typed column is untouched"
psql_axiom "UPDATE k8s.example_com_widgets SET raw = jsonb_set(raw, '{spec,size}', '42') WHERE namespace = '$NS' AND name = 'flange';"
want="$(kubectl_e2e -n "$NS" get widget flange -o jsonpath='{.spec.size}|{.spec.color}')"
[[ "$want" == "42|orange" ]] || fail "cluster shows '$want' after raw jsonb_set, want '42|orange'"

log "renaming is feature_not_supported (0A000); server-managed columns are refused"
got="$(psql_axiom "DO \$\$ BEGIN UPDATE k8s.example_com_widgets SET name = 'renamed' WHERE namespace = '$NS' AND name = 'flange'; RAISE EXCEPTION 'unexpected success';
  EXCEPTION WHEN feature_not_supported THEN RAISE NOTICE 'caught %', SQLSTATE; END \$\$;" 2>&1 || true)"
grep -q "caught 0A000" <<<"$got" || fail "renaming should raise 0A000, got: $got"
got="$(psql_axiom "DO \$\$ BEGIN UPDATE k8s.example_com_widgets SET uid = 'forged' WHERE namespace = '$NS' AND name = 'flange'; RAISE EXCEPTION 'unexpected success';
  EXCEPTION WHEN feature_not_supported THEN RAISE NOTICE 'caught %', SQLSTATE; END \$\$;" 2>&1 || true)"
grep -q "caught 0A000" <<<"$got" || fail "writing uid should raise 0A000, got: $got"

log "a concurrent kubectl change between read and write is serialization_failure (40001)"
# pg_sleep in the WHERE runs after the row (and its resourceVersion) was read
# but before ExecForeignUpdate sends it; kubectl changes the object meanwhile.
# It must reference a column: a Var-free qual would be a gating qual evaluated
# once *before* the scan, and the read would then see the patched object.
( sleep 2; kubectl_e2e -n "$NS" patch widget flange --type merge -p '{"spec":{"color":"outofband"}}' >/dev/null ) &
patcher=$!
got="$(psql_axiom "DO \$\$ BEGIN UPDATE k8s.example_com_widgets SET spec = spec || '{\"from_sql\":\"yes\"}' WHERE namespace = '$NS' AND name = 'flange' AND pg_sleep(6 + length(name) * 0) IS NOT NULL;
  RAISE EXCEPTION 'unexpected success'; EXCEPTION WHEN serialization_failure THEN RAISE NOTICE 'caught % %', SQLSTATE, SQLERRM; END \$\$;" 2>&1 || true)"
wait $patcher
grep -q "caught 40001" <<<"$got" || fail "expected 40001 serialization_failure, got: $got"
# The out-of-band change survived: the stale write was refused, not merged over.
want="$(kubectl_e2e -n "$NS" get widget flange -o jsonpath='{.spec.color}')"
[[ "$want" == "outofband" ]] || fail "stale write overwrote the concurrent change: spec.color is '$want'"
echo "conflict surfaced as 40001, concurrent change preserved"

log "DELETE removes it from the cluster"
psql_axiom "DELETE FROM k8s.example_com_widgets WHERE namespace = '$NS' AND name = 'flange';"
kubectl_e2e -n "$NS" get widget flange >/dev/null 2>&1 && fail "widget still exists after DELETE"
echo "flange is gone"

# --- Phase 3 equivalent: the watch cache --------------------------------------------

log "a watch table over the CRD reaches ACTIVE and serves from the cache"
psql_axiom "CREATE FOREIGN TABLE k8s.widgets_live (name text, namespace text, spec jsonb, raw jsonb) SERVER kind OPTIONS (resource 'widgets', group 'example.com', version 'v1', kind 'Widget', cache_mode 'watch');"
psql_axiom "SELECT count(*) FROM k8s.widgets_live;" >/dev/null   # first scan requests the subscription

wait_state() {
  local want="$1" timeout="${2:-90}" state
  for _ in $(seq "$timeout"); do
    state="$(psql_axiom "SELECT state FROM axiom_watch_status() WHERE resource = 'widgets.example.com' LIMIT 1;" 2>/dev/null || true)"
    [[ "$state" == "$want" ]] && return 0
    sleep 1
  done
  fail "watch state for widgets is '$state', want '$want' within ${timeout}s"
}
wait_state ACTIVE 120
echo "subscription is ACTIVE"

# Counters, not log lines: a Pod restart starts a fresh log, and this gate
# restarts nothing but the cluster gate does. axiom_gateway_stats() asks the
# gateway process directly.
lists_now() { gw_lists; }
L0="$(lists_now)"

log "a kubectl-side change appears in SQL within watch latency, with no new List"
kubectl_e2e -n "$NS" create -f - >/dev/null <<EOF
apiVersion: example.com/v1
kind: Widget
metadata:
  name: watched
  namespace: $NS
spec:
  size: 99
  color: violet
EOF
for _ in $(seq 60); do
  got="$(psql_axiom "SELECT spec->>'size' FROM k8s.widgets_live WHERE namespace = '$NS' AND name = 'watched';" 2>/dev/null || true)"
  [[ "$got" == "99" ]] && break
  sleep 1
done
[[ "$got" == "99" ]] || fail "watched widget did not reach the cache: '$got'"
[[ "$(lists_now)" == "$L0" ]] || fail "cache-served scans issued Lists: $(lists_now) != $L0"
echo "served from cache, List count unchanged at $L0"

log "a delete is reflected too"
kubectl_e2e -n "$NS" delete widget watched >/dev/null
for _ in $(seq 60); do
  got="$(psql_axiom "SELECT count(*) FROM k8s.widgets_live WHERE namespace = '$NS' AND name = 'watched';" 2>/dev/null || true)"
  [[ "$got" == "0" ]] && break
  sleep 1
done
[[ "$got" == "0" ]] || fail "deleted widget still in the cache"

log "NOTIFY carries the CRD's resource name"
( sleep 3; kubectl_e2e -n "$NS" create -f - >/dev/null <<EOF
apiVersion: example.com/v1
kind: Widget
metadata:
  name: notified
  namespace: $NS
spec:
  size: 1
EOF
) &
out="$(printf 'LISTEN axiom_events;\nSELECT pg_sleep(10);\nSELECT 1;\n' | compose exec -T "$E2E_SVC_POSTGRES" psql -U "$E2E_PG_USER" -d "$E2E_PG_DB" -At 2>&1)"
wait
grep -q '"resource":"widgets.example.com"' <<<"$out" || fail "payload lacks the CRD resource name: $out"
grep -q '"name":"notified"' <<<"$out" || fail "payload lacks the object name: $out"
echo "$(grep -m1 'payload' <<<"$out")"

# --- discovery and allowlist behaviour ----------------------------------------------

log "a CRD created after the gateway started is discoverable without a restart"
kubectl_e2e apply -f - >/dev/null <<'EOF'
apiVersion: apiextensions.k8s.io/v1
kind: CustomResourceDefinition
metadata:
  name: gizmos.example.com
spec:
  group: example.com
  scope: Namespaced
  names: {plural: gizmos, singular: gizmo, kind: Gizmo, listKind: GizmoList}
  versions:
    - name: v1
      served: true
      storage: true
      schema:
        openAPIV3Schema:
          type: object
          properties:
            spec: {type: object, properties: {note: {type: string}}}
EOF
kubectl_e2e wait --for=condition=Established crd/gizmos.example.com --timeout=60s >/dev/null || fail "gizmos CRD not Established"
# The gateway's --serve list covers widgets.example.com only, so gizmos must NOT
# appear: the allowlist bounds what is offered, independently of discovery.
psql_axiom "DROP SCHEMA IF EXISTS late CASCADE;" >/dev/null
psql_axiom "CREATE SCHEMA late;"
psql_axiom "IMPORT FOREIGN SCHEMA k8s FROM SERVER kind INTO late;" 2>&1 | grep -v '^$' || true
got="$(psql_axiom "SELECT string_agg(c.relname, ',' ORDER BY c.relname) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = 'late' AND c.relkind = 'f';")"
[[ "$got" == "core_configmaps,core_pods,example_com_widgets" ]] || fail "import after CRD creation gave '$got', want 'core_configmaps,core_pods,example_com_widgets'"
grep -q "gizmos" <<<"$got" && fail "gizmos is outside --serve but was offered anyway"
echo "gizmos correctly withheld: the allowlist, not discovery, bounds what is served"

log "a kind outside --serve is refused at scan time too"
psql_axiom "CREATE FOREIGN TABLE k8s.gizmos (name text, namespace text, spec jsonb, raw jsonb) SERVER kind OPTIONS (resource 'gizmos', group 'example.com', version 'v1', kind 'Gizmo');"
got="$(psql_axiom "DO \$\$ BEGIN PERFORM count(*) FROM k8s.gizmos; RAISE EXCEPTION 'unexpected success';
  EXCEPTION WHEN OTHERS THEN RAISE NOTICE 'caught % %', SQLSTATE, SQLERRM; END \$\$;" 2>&1 || true)"
grep -q "caught" <<<"$got" || fail "a hand-written table for an unserved kind should fail at scan: $got"
grep -qi "unsupported kind" <<<"$got" || fail "the error should say the kind is unsupported: $got"
# And it tells the operator what to do. A table for a kind the gateway stopped
# serving is the confusing case: it is still in the catalog and simply fails,
# because foreign tables do not follow a configuration change.
grep -qi "does not serve this kind" <<<"$got" \
  || fail "the error should explain what happened: $got"
grep -qi "IMPORT FOREIGN SCHEMA" <<<"$got" \
  || fail "the error should tell the operator to re-import: $got"
# It must still not say *which* of the three causes applies, or the allowlist
# becomes enumerable by anyone who can define a foreign table.
grep -qi "excludes it" <<<"$got" \
  || fail "the error should name all the possible causes, not the actual one: $got"
echo "scan refused with an actionable message that does not reveal the cause"

log "RBAC is least-privilege: the gateway identity cannot reach kinds it does not serve"
# `kubectl auth can-i` exits 1 on "no", which would trip set -e; swallow the
# status and assert on the printed answer instead.
can_i() { kubectl_e2e --as="$SA" auth can-i "$1" "$2" -n "$NS" 2>/dev/null || true; }
[[ "$(can_i list secrets)" == "no" ]] || fail "gateway SA can list secrets"
[[ "$(can_i list gizmos.example.com)" == "no" ]] || fail "gateway SA can list gizmos (outside --serve)"
[[ "$(can_i list widgets.example.com)" == "yes" ]] || fail "gateway SA cannot list widgets"
[[ "$(can_i delete widgets.example.com)" == "yes" ]] || fail "gateway SA cannot delete widgets"
echo "secrets=no gizmos=no widgets=yes"

log "IMPORT filters: LIMIT TO and a group-named remote schema"
psql_axiom "DROP SCHEMA IF EXISTS onlyw CASCADE;" >/dev/null
psql_axiom "CREATE SCHEMA onlyw;"
psql_axiom "IMPORT FOREIGN SCHEMA \"example.com\" FROM SERVER kind INTO onlyw;"
got="$(psql_axiom "SELECT string_agg(c.relname, ',' ORDER BY c.relname) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = 'onlyw' AND c.relkind = 'f';")"
[[ "$got" == "example_com_widgets" ]] || fail "group-scoped import gave '$got', want 'example_com_widgets'"
echo "example.com import: $got"

psql_axiom "DROP SCHEMA IF EXISTS limited CASCADE;" >/dev/null
psql_axiom "CREATE SCHEMA limited;"
psql_axiom "IMPORT FOREIGN SCHEMA k8s LIMIT TO (example_com_widgets) FROM SERVER kind INTO limited;"
got="$(psql_axiom "SELECT string_agg(c.relname, ',' ORDER BY c.relname) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = 'limited' AND c.relkind = 'f';")"
[[ "$got" == "example_com_widgets" ]] || fail "LIMIT TO by table name gave '$got'"
# The spelling from before names carried their group imports nothing -- Postgres
# filters LIMIT TO by table name -- so it has to say what was probably meant.
psql_axiom "DROP SCHEMA limited CASCADE;" >/dev/null
psql_axiom "CREATE SCHEMA limited;"
out="$(psql_axiom "IMPORT FOREIGN SCHEMA k8s LIMIT TO (widgets) FROM SERVER kind INTO limited;" 2>&1)"
grep -q 'did you mean example_com_widgets?' <<<"$out" \
  || fail "LIMIT TO (widgets) should suggest example_com_widgets; got: $out"
psql_axiom "DROP SCHEMA limited CASCADE;" >/dev/null
echo "LIMIT TO names tables, and a bare plural gets a hint"

log "a CRD deleted from the cluster stops being offered once the cache expires"
# The opposite direction from "a new CRD needs no restart", which is covered
# above. A resource list fetched successfully used to be kept for the life of
# the process, so a kind that was uninstalled kept resolving and IMPORT kept
# generating a table for it. Found by uninstalling an operator and re-importing.
#
# This is the case a unit test cannot reach: the fix has to drop client-go's own
# discovery cache as well as ours, and only a real cached discovery client shows
# whether it does.
kubectl_e2e delete crd widgets.example.com --ignore-not-found --wait=true >/dev/null \
  || fail "failed to delete the widget CRD"

# Re-import until widgets is gone, not until the import merely succeeds. The
# first import after the delete succeeds while the cache is still warm and
# still offers widgets, so a retry-until-success loop exits immediately with
# the answer it was meant to wait out.
got=""
imported=0
last_err=""
deadline=$((SECONDS + 60))
while (( SECONDS < deadline )); do
  psql_axiom "DROP SCHEMA IF EXISTS gone CASCADE;" >/dev/null 2>&1
  psql_axiom "CREATE SCHEMA gone;" >/dev/null 2>&1
  # The import must succeed before its result means anything. A failed import
  # leaves `gone` empty, which is indistinguishable from "widgets is no longer
  # offered" -- so a gateway that had crashed would satisfy this assertion
  # without the TTL doing anything.
  if ! last_err="$(psql_axiom "IMPORT FOREIGN SCHEMA \"example.com\" FROM SERVER kind INTO gone;" 2>&1)"; then
    sleep 2
    continue
  fi
  imported=1
  got="$(psql_axiom "SELECT coalesce(string_agg(c.relname, ',' ORDER BY c.relname), '') FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = 'gone' AND c.relkind = 'f';")"
  grep -q "widgets" <<<"$got" || break
  sleep 2
done
(( imported == 1 )) \
  || fail $'no IMPORT succeeded within 60s, so the TTL was never exercised; last error:\n'"$last_err"
grep -q "widgets" <<<"$got" \
  && fail "a deleted CRD was still offered 60s after the 3s discovery TTL: '$got'"
echo "deleted CRD no longer offered (import produced: '${got:-nothing}')"
psql_axiom "DROP SCHEMA IF EXISTS gone CASCADE;" >/dev/null

log "cleanup"
kubectl_e2e -n "$NS" delete widgets --all --ignore-not-found --wait=false >/dev/null
kubectl_e2e delete crd gizmos.example.com --ignore-not-found --wait=false >/dev/null

log "CRD E2E PASSED"
