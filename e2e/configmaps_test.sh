#!/usr/bin/env bash
# ConfigMaps E2E (Phase 2 in docs/PLAN.md): SQL DML on a k8s_configmaps foreign
# table mutates a kind cluster, with optimistic-concurrency conflicts surfaced
# as a distinct SQLSTATE. Runs after the Phase 1 gate; reuses its cluster if
# E2E_KIND_KEEP left one behind.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$here/lib/stack.sh"
source "$here/lib/kind.sh"

E2E_COMPOSE_OVERLAYS="${E2E_COMPOSE_OVERLAYS:-} $E2E_ROOT/deploy/compose/docker-compose.kind.yml"
NS="axiom-e2e"
SA="system:serviceaccount:$E2E_GATEWAY_SA_NS:$E2E_GATEWAY_SA"

kind_up
e2e_on_teardown kind_down
stack_up

log "namespace and DDL"
kubectl_e2e create namespace "$NS" --dry-run=client -o yaml | kubectl_e2e apply -f - >/dev/null
kubectl_e2e -n "$NS" delete configmap --all --ignore-not-found >/dev/null
psql_axiom "CREATE EXTENSION IF NOT EXISTS axiom;"
psql_axiom "DROP SERVER IF EXISTS kind CASCADE;"
psql_axiom "CREATE SERVER kind FOREIGN DATA WRAPPER axiom_fdw OPTIONS (endpoint '$E2E_GATEWAY_ENDPOINT', ca_cert '/certs/ca.crt', rpc_timeout_secs '10');"
psql_axiom "CREATE FOREIGN TABLE k8s_configmaps (name text, namespace text, data jsonb, raw jsonb) SERVER kind OPTIONS (resource 'configmaps');"
psql_axiom "CREATE FOREIGN TABLE k8s_pods (name text, namespace text, phase text, node text, raw jsonb) SERVER kind OPTIONS (resource 'pods');"

log "INSERT creates the ConfigMap in the cluster (RETURNING shows server-assigned uid)"
uid="$(psql_axiom "INSERT INTO k8s_configmaps (name, namespace, data) VALUES ('app-config', '$NS', '{\"LOG_LEVEL\":\"info\",\"REPLICAS\":\"2\"}') RETURNING raw->'metadata'->>'uid';")"
[[ -n "$uid" ]] || fail "INSERT RETURNING gave no uid"
want="$(kubectl_e2e -n "$NS" get configmap app-config -o jsonpath='{.metadata.uid}|{.data.LOG_LEVEL}|{.data.REPLICAS}')"
[[ "$want" == "$uid|info|2" ]] || fail "cluster shows '$want', want '$uid|info|2'"
echo "created uid=$uid"

log "duplicate INSERT is unique_violation (23505)"
got="$(psql_axiom "DO \$\$ BEGIN INSERT INTO k8s_configmaps (name, namespace) VALUES ('app-config', '$NS'); RAISE EXCEPTION 'unexpected success';
  EXCEPTION WHEN unique_violation THEN RAISE NOTICE 'caught %', SQLSTATE; END \$\$;" 2>&1 || true)"
grep -q "caught 23505" <<<"$got" || fail "expected 23505, got: $got"

log "UPDATE SET data is reflected in the cluster"
psql_axiom "UPDATE k8s_configmaps SET data = data || '{\"LOG_LEVEL\":\"debug\"}' WHERE namespace = '$NS' AND name = 'app-config';"
want="$(kubectl_e2e -n "$NS" get configmap app-config -o jsonpath='{.data.LOG_LEVEL}|{.data.REPLICAS}')"
[[ "$want" == "debug|2" ]] || fail "cluster shows '$want' after UPDATE, want 'debug|2'"
got="$(psql_axiom "SELECT data->>'LOG_LEVEL' FROM k8s_configmaps WHERE namespace = '$NS' AND name = 'app-config';")"
[[ "$got" == "debug" ]] || fail "SELECT after UPDATE shows '$got'"

log "UPDATE via raw preserves metadata the SQL user did not touch (labels) and cannot rename"
kubectl_e2e -n "$NS" label configmap app-config tier=backend --overwrite >/dev/null
psql_axiom "UPDATE k8s_configmaps SET data = '{\"LOG_LEVEL\":\"warn\"}' WHERE namespace = '$NS' AND name = 'app-config';"
want="$(kubectl_e2e -n "$NS" get configmap app-config -o jsonpath='{.metadata.labels.tier}|{.data.LOG_LEVEL}|{.data.REPLICAS}')"
[[ "$want" == "backend|warn|" ]] || fail "cluster shows '$want', want 'backend|warn|' (labels kept, data replaced)"
got="$(psql_axiom "DO \$\$ BEGIN UPDATE k8s_configmaps SET name = 'renamed' WHERE namespace = '$NS' AND name = 'app-config'; RAISE EXCEPTION 'unexpected success';
  EXCEPTION WHEN feature_not_supported THEN RAISE NOTICE 'caught %', SQLSTATE; END \$\$;" 2>&1 || true)"
grep -q "caught 0A000" <<<"$got" || fail "renaming should raise 0A000, got: $got"

log "concurrent out-of-band change between read and write is serialization_failure (40001), not a silent overwrite"
# pg_sleep in the WHERE runs after the row (and its resourceVersion) was read
# but before ExecForeignUpdate sends it; kubectl changes the object meanwhile.
# It must reference a column: a Var-free qual would be a gating qual evaluated
# once *before* the scan, and the read would then see the patched object.
( sleep 2; kubectl_e2e -n "$NS" patch configmap app-config --type merge -p '{"data":{"OUT_OF_BAND":"yes"}}' >/dev/null ) &
patcher=$!
got="$(psql_axiom "DO \$\$ BEGIN UPDATE k8s_configmaps SET data = data || '{\"FROM_SQL\":\"yes\"}' WHERE namespace = '$NS' AND name = 'app-config' AND pg_sleep(6 + length(name) * 0) IS NOT NULL;
  RAISE EXCEPTION 'unexpected success'; EXCEPTION WHEN serialization_failure THEN RAISE NOTICE 'caught % %', SQLSTATE, SQLERRM; END \$\$;" 2>&1 || true)"
wait $patcher
grep -q "caught 40001" <<<"$got" || fail "expected 40001 serialization_failure, got: $got"
want="$(kubectl_e2e -n "$NS" get configmap app-config -o jsonpath='{.data.OUT_OF_BAND}|{.data.FROM_SQL}')"
[[ "$want" == "yes|" ]] || fail "out-of-band write was overwritten or SQL write leaked: '$want'"
echo "$got" | grep "caught"

log "retry after re-read succeeds (the conflict is retryable)"
psql_axiom "UPDATE k8s_configmaps SET data = data || '{\"FROM_SQL\":\"yes\"}' WHERE namespace = '$NS' AND name = 'app-config';"
want="$(kubectl_e2e -n "$NS" get configmap app-config -o jsonpath='{.data.OUT_OF_BAND}|{.data.FROM_SQL}')"
[[ "$want" == "yes|yes" ]] || fail "retry did not merge: '$want'"

log "writes are never cache-served: every DML is a gateway RPC (gateway log has create/update/delete lines)"
glogs="$(stack_logs "$E2E_SVC_GATEWAY")"
grep -q "\"msg\":\"create\".*\"name\":\"app-config\"" <<<"$glogs" || fail "no create in gateway log"
[[ "$(grep -c "\"msg\":\"update\".*\"name\":\"app-config\"" <<<"$glogs")" -ge 3 ]] || fail "expected >=3 successful updates in gateway log"

log "Pods table is read-only at the SQL layer"
got="$(psql_axiom "DO \$\$ BEGIN DELETE FROM k8s_pods WHERE namespace = 'kube-system'; RAISE EXCEPTION 'unexpected success';
  EXCEPTION WHEN OTHERS THEN RAISE NOTICE 'caught % %', SQLSTATE, SQLERRM; END \$\$;" 2>&1 || true)"
grep -qiE "caught 55000 .*(does not allow deletes|cannot delete from foreign table)" <<<"$got" || fail "DELETE on pods should be rejected by Postgres (SQLSTATE 55000), got: $got"
[[ -n "$(kubectl_e2e -n kube-system get pods --no-headers 2>/dev/null)" ]] || fail "kube-system pods vanished?!"

log "DELETE removes the ConfigMap from the cluster"
psql_axiom "DELETE FROM k8s_configmaps WHERE namespace = '$NS' AND name = 'app-config';"
if kubectl_e2e -n "$NS" get configmap app-config >/dev/null 2>&1; then fail "configmap still exists after DELETE"; fi
got="$(psql_axiom "SELECT count(*) FROM k8s_configmaps WHERE namespace = '$NS' AND name = 'app-config';")"
[[ "$got" == "0" ]] || fail "SELECT after DELETE returned $got rows"

log "RBAC: gateway identity may write configmaps but nothing else"
[[ "$(kubectl_e2e --as="$SA" auth can-i create configmaps -n "$NS" 2>/dev/null)" == "yes" ]] || fail "SA cannot create configmaps"
[[ "$(kubectl_e2e --as="$SA" auth can-i delete configmaps -n "$NS" 2>/dev/null)" == "yes" ]] || fail "SA cannot delete configmaps"
[[ "$(kubectl_e2e --as="$SA" auth can-i create pods -n "$NS" 2>/dev/null)" == "no" ]] || fail "SA can create pods"
[[ "$(kubectl_e2e --as="$SA" auth can-i get secrets -n "$NS" 2>/dev/null)" == "no" ]] || fail "SA can read secrets"

log "CONFIGMAPS E2E PASSED"
