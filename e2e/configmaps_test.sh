#!/usr/bin/env bash
# ConfigMaps E2E (Phase 2 in docs/PLAN.md): SQL DML on a k8s_configmaps foreign
# table mutates a kind cluster, with optimistic-concurrency conflicts surfaced
# as a distinct SQLSTATE. Runs after the Phase 1 gate; reuses its cluster if
# E2E_KIND_KEEP left one behind.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# Must precede the source: stack.sh resolves the gateway endpoint from it.
E2E_GATEWAY_MODE=incluster

source "$here/lib/stack.sh"
source "$here/lib/kind.sh"

E2E_COMPOSE_OVERLAYS="${E2E_COMPOSE_OVERLAYS:-} $E2E_ROOT/deploy/compose/docker-compose.kind.yml $E2E_ROOT/deploy/compose/docker-compose.incluster.yml"
NS="axiom-e2e"
SA="system:serviceaccount:$E2E_GATEWAY_SA_NS:$E2E_GATEWAY_SA"

kind_up
e2e_on_teardown kind_down
stack_up
# A cluster-scoped CRD the gateway may write, for the raw-namespace checks
# below. Established before the gateway starts, so discovery sees it.
kubectl_e2e apply -f "$here/fixtures/clustergadget-crd.yaml" >/dev/null || fail "apply the ClusterGadget CRD"
clustergadgets_down() {
  kubectl_e2e delete clustergadgets --all --ignore-not-found >/dev/null 2>&1 || true
  kubectl_e2e delete -f "$here/fixtures/clustergadget-crd.yaml" --ignore-not-found --wait=false >/dev/null 2>&1 || true
}
e2e_on_teardown clustergadgets_down
kubectl_e2e wait --for=condition=Established crd/clustergadgets.example.com --timeout=60s >/dev/null \
  || fail "the ClusterGadget CRD did not become Established"
kind_deploy_gateway "pods,configmaps,clustergadgets.example.com"

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

log "UPDATE through raw (jsonb_set) changes data even though the typed data column is untouched"
psql_axiom "UPDATE k8s_configmaps SET raw = jsonb_set(raw, '{data,VIA_RAW}', '\"1\"') WHERE namespace = '$NS' AND name = 'app-config';"
want="$(kubectl_e2e -n "$NS" get configmap app-config -o jsonpath='{.data.LOG_LEVEL}|{.data.VIA_RAW}')"
[[ "$want" == "warn|1" ]] || fail "cluster shows '$want' after raw jsonb_set, want 'warn|1'"

log "SET data = NULL clears data; SET name = NULL is not_null_violation (23502)"
psql_axiom "UPDATE k8s_configmaps SET data = NULL WHERE namespace = '$NS' AND name = 'app-config';"
want="$(kubectl_e2e -n "$NS" get configmap app-config -o jsonpath='{.data}')"
[[ -z "$want" || "$want" == "{}" ]] || fail "data not cleared: '$want'"
got="$(psql_axiom "DO \$\$ BEGIN UPDATE k8s_configmaps SET name = NULL WHERE namespace = '$NS' AND name = 'app-config'; RAISE EXCEPTION 'unexpected success';
  EXCEPTION WHEN not_null_violation THEN RAISE NOTICE 'caught %', SQLSTATE; END \$\$;" 2>&1 || true)"
grep -q "caught 23502" <<<"$got" || fail "expected 23502, got: $got"
psql_axiom "UPDATE k8s_configmaps SET data = '{\"LOG_LEVEL\":\"warn\"}' WHERE namespace = '$NS' AND name = 'app-config';"

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

# Regression gate for issue #51, which shipped in v0.1.0.
#
# The gateway pings a peer idle for 30s and drops it 10s later. A backend
# cannot answer -- unary channels are WhileActive on a current_thread runtime,
# so between queries nothing polls the connection -- so the gateway is
# guaranteed to close a connection a backend leaves idle for 40s, and the next
# statement used to be the one that found out. Inside a transaction that lost
# the work.
#
# Only an E2E can catch this: it needs a real gateway running its real
# keepalive timers. A unit test can assert the constant, and one does, but the
# constant being right is not the same as the connection surviving.
#
# The sleep is why this costs 45s and cannot be shortened: the gateway's timers
# are compiled in, not flags, so there is no way to make it decide sooner.
#
# One psql invocation on purpose. `psql_axiom` opens a connection per call, and
# a fresh connection has no cached channel, so running these as separate calls
# would pass on a broken build -- there would be nothing stale to meet.
log "a session idle past the gateway's keepalive window still works (issue #51)"
idle_out="$(compose exec -T "$E2E_SVC_POSTGRES" psql -v ON_ERROR_STOP=1 \
  -U "$E2E_PG_USER" -d "$E2E_PG_DB" -At <<SQL 2>&1 || true
SELECT count(*) FROM k8s_configmaps WHERE namespace = '$NS';
SELECT pg_sleep(45);
INSERT INTO k8s_configmaps (namespace, name, data)
  VALUES ('$NS', 'after-idle', '{"k":"v"}');
SELECT count(*) FROM k8s_configmaps WHERE namespace = '$NS' AND name = 'after-idle';
SQL
)"
grep -q 'cannot reach gateway' <<<"$idle_out" \
  && fail "a statement after the idle window could not reach the gateway: $idle_out"
# The write is the half that matters most: reads could be papered over by
# retrying, a write cannot, so this is what proves the connection was rebuilt
# rather than the failure swallowed.
kubectl_e2e -n "$NS" get configmap after-idle >/dev/null 2>&1 \
  || fail "INSERT after the idle window did not reach the cluster: $idle_out"
# And the read-back, asserted rather than assumed. Without this the final
# SELECT is decoration: it could return 0, or fail for a reason that does not
# mention the gateway, and nothing above would notice.
[[ "$(tail -n1 <<<"$idle_out")" == "1" ]] \
  || fail "reading back the row written after the idle window did not return 1: $idle_out"
echo "wrote and read back across a 45s idle gap"
kubectl_e2e -n "$NS" delete configmap after-idle >/dev/null 2>&1 || true

log "INSERT honours raw: a whole manifest arrives intact (#78)"
# Read back from the cluster, not through Axiom: Axiom would report an empty
# object consistently, because the object really would be empty.
psql_axiom "INSERT INTO k8s_configmaps (namespace, name, raw) VALUES ('$NS', 'from-raw',
  '{\"apiVersion\":\"v1\",\"kind\":\"ConfigMap\",
    \"metadata\":{\"name\":\"from-raw\",\"namespace\":\"$NS\",
                \"labels\":{\"from\":\"raw\"},\"annotations\":{\"note\":\"raw\"}},
    \"data\":{\"k\":\"v\"}}');"
got="$(kubectl_e2e -n "$NS" get configmap from-raw \
  -o jsonpath='{.metadata.labels.from}|{.metadata.annotations.note}|{.data.k}')"
[[ "$got" == "raw|raw|v" ]] || fail "#78: the cluster has '$got', want 'raw|raw|v' from raw"
echo "labels, annotations and data all reached the cluster"

log "raw as a template: a typed column overrides the same field, the rest comes from raw"
psql_axiom "INSERT INTO k8s_configmaps (namespace, name, data, raw)
  SELECT '$NS', 'from-template', '{\"k\":\"override\"}', raw
    FROM k8s_configmaps WHERE namespace = '$NS' AND name = 'from-raw';"
got="$(kubectl_e2e -n "$NS" get configmap from-template \
  -o jsonpath='{.metadata.labels.from}|{.data.k}')"
[[ "$got" == "raw|override" ]] || fail "template insert gave '$got', want 'raw|override'"
echo "copied from-raw with data overridden: $got"

log "raw of another kind is refused, not relabelled"
out="$(psql_axiom "INSERT INTO k8s_configmaps (namespace, name, raw) VALUES ('$NS', 'not-a-cm',
  '{\"apiVersion\":\"apps/v1\",\"kind\":\"Deployment\"}');" 2>&1 || true)"
grep -q "raw describes apps/v1 Deployment" <<<"$out" || fail "a Deployment manifest was accepted: $out"
kubectl_e2e -n "$NS" get configmap not-a-cm >/dev/null 2>&1 && fail "the mismatched manifest created a ConfigMap"
echo "refused: $(grep -o 'raw describes [^,]*' <<<"$out" | head -1)"

log "UPDATE refuses raw replaced with another kind, and leaves the object alone"
out="$(psql_axiom "UPDATE k8s_configmaps SET raw = jsonb_set(raw, '{kind}', '\"Secret\"')
                    WHERE namespace = '$NS' AND name = 'from-raw';" 2>&1 || true)"
grep -q "raw describes v1 Secret" <<<"$out" || fail "an UPDATE turning raw into a Secret was accepted: $out"
got="$(kubectl_e2e -n "$NS" get configmap from-raw -o jsonpath='{.kind}|{.data.k}')"
[[ "$got" == "ConfigMap|v" ]] || fail "the refused UPDATE still changed the object: '$got'"
echo "refused, and from-raw is untouched: $got"
kubectl_e2e -n "$NS" delete configmap from-raw from-template --ignore-not-found >/dev/null

log "a cluster-scoped kind drops a namespace that raw claims, on INSERT and UPDATE"
# The gateway refuses a body whose metadata.namespace differs from the
# request's, and a cluster-scoped request has none. A raw copied from a
# namespaced manifest, or written by hand, easily carries one.
kubectl_e2e delete clustergadget g1 --ignore-not-found >/dev/null
psql_axiom "CREATE FOREIGN TABLE IF NOT EXISTS k8s_clustergadgets (name text, spec jsonb, raw jsonb) SERVER kind
            OPTIONS (resource 'clustergadgets', group 'example.com', version 'v1', kind 'ClusterGadget', namespaced 'false');"
psql_axiom "INSERT INTO k8s_clustergadgets (raw) VALUES
  ('{\"metadata\":{\"name\":\"g1\",\"namespace\":\"default\"},\"spec\":{\"size\":\"s\"}}');" \
  || fail "INSERT of a cluster-scoped raw that names a namespace failed"
got="$(kubectl_e2e get clustergadget g1 -o jsonpath='{.metadata.name}|{.metadata.namespace}|{.spec.size}')"
[[ "$got" == "g1||s" ]] || fail "the cluster has '$got', want 'g1||s'"
psql_axiom "UPDATE k8s_clustergadgets
               SET raw = jsonb_set(jsonb_set(raw, '{metadata,namespace}', '\"default\"'), '{spec,size}', '\"m\"')
             WHERE name = 'g1';" \
  || fail "UPDATE of a cluster-scoped raw that names a namespace failed"
got="$(kubectl_e2e get clustergadget g1 -o jsonpath='{.spec.size}')"
[[ "$got" == "m" ]] || fail "the UPDATE did not reach the cluster: spec.size is '$got'"
kubectl_e2e delete clustergadget g1 --ignore-not-found >/dev/null
echo "inserted and updated with a namespace in raw; the cluster-scoped object has none"

log "a later page larger than the first shrinks instead of failing the listing (#85)"
# Tiny ConfigMaps named a-*, then large ones named b-*. A namespaced list comes
# back in name order, so at the extension's page size of 200 the first page is
# all tiny and the second is about 9 MiB -- over the 4 MiB response budget. Only
# a real kube-apiserver shows that a continuation re-requested at a smaller
# limit resumes where its token points; the unit tests' fake cannot.
PAGE_NS="axiom-paging"
kubectl_e2e delete namespace "$PAGE_NS" --ignore-not-found --wait=true >/dev/null
kubectl_e2e create namespace "$PAGE_NS" >/dev/null
paging_down() { kubectl_e2e delete namespace "$PAGE_NS" --ignore-not-found --wait=false >/dev/null 2>&1 || true; }
e2e_on_teardown paging_down
for i in $(seq -w 0 199); do
  printf 'apiVersion: v1\nkind: ConfigMap\nmetadata: {name: a-%s}\ndata: {k: v}\n---\n' "$i"
done | kubectl_e2e -n "$PAGE_NS" create -f - >/dev/null || fail "create the small ConfigMaps"
blob="$(mktemp)"
head -c 921600 /dev/zero | tr '\0' 'x' > "$blob"
for i in $(seq -w 0 9); do
  # create, not apply: apply's last-applied annotation would double the size
  # past the 1 MiB object limit.
  kubectl_e2e -n "$PAGE_NS" create configmap "b-$i" --from-file=blob="$blob" >/dev/null \
    || fail "create large ConfigMap b-$i"
done
rm -f "$blob"
want="$(kubectl_e2e -n "$PAGE_NS" get configmaps --no-headers | wc -l | tr -d ' ')"
got="$(psql_axiom "SELECT count(*) || '|' || count(DISTINCT raw->'metadata'->>'uid')
                     FROM k8s_configmaps WHERE namespace = '$PAGE_NS';")" \
  || fail "#85: listing a namespace whose second page outgrows the first failed"
[[ "$got" == "$want|$want" ]] \
  || fail "SQL saw '$got' (count|distinct uids), kubectl sees $want: an object was lost or repeated"
stack_logs "$E2E_SVC_GATEWAY" | grep '"msg":"list_page_shrunk"' | grep -q ConfigMap \
  || fail "the gateway never logged list_page_shrunk, so this did not exercise a shrunk continuation"
echo "$want ConfigMaps, each exactly once, across a shrunk continuation"

log "CONFIGMAPS E2E PASSED"
