#!/usr/bin/env bash
# Metrics and events E2E: with a kind cluster running metrics-server, prove
# that `metrics.k8s.io` and the two events API groups reach SQL through the
# gateway's own ServiceAccount, that the numbers match `kubectl top`, and that
# axiom_quantity() turns Kubernetes quantity strings into numbers you can
# compare and sum.
#
# The ServiceAccount is the point. Local development uses a cluster-admin
# kubeconfig, so metrics appearing there proves nothing about a real
# deployment. This gate also pins the shape of the shipped read grant in
# deploy/k8s/gateway-rbac.yaml: broad -- workloads, networking, cluster-scoped
# kinds, custom resources through an aggregation label -- and never Secrets.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
E2E_GATEWAY_MODE=incluster

source "$here/lib/stack.sh"
source "$here/lib/kind.sh"

E2E_COMPOSE_OVERLAYS="${E2E_COMPOSE_OVERLAYS:-} $E2E_ROOT/deploy/compose/docker-compose.kind.yml $E2E_ROOT/deploy/compose/docker-compose.incluster.yml"
NS="axiom-e2e"

SA="system:serviceaccount:$E2E_GATEWAY_SA_NS:$E2E_GATEWAY_SA"
can_i() { kubectl_e2e auth can-i "$@" --as="$SA" 2>/dev/null || true; }
# can_i_group VERB GROUP RESOURCE -> yes/no, from a SubjectAccessReview with
# the group spelled out. `kubectl auth can-i` resolves the name through
# discovery, and for a group the cluster does not serve it falls back to
# checking the whole string as a *core* resource -- answering "no" for any API
# not installed, however RBAC reads. Grants for APIs that arrive later are
# exactly what this gate needs to check.
can_i_group() {
  local allowed
  allowed="$(kubectl_e2e create -o jsonpath='{.status.allowed}' -f - 2>/dev/null <<YAML || true
apiVersion: authorization.k8s.io/v1
kind: SubjectAccessReview
spec:
  user: "$SA"
  groups: ["system:serviceaccounts", "system:serviceaccounts:$E2E_GATEWAY_SA_NS", "system:authenticated"]
  resourceAttributes: {verb: "$1", group: "$2", resource: "$3"}
YAML
)"
  [[ "$allowed" == "true" ]] && echo yes || echo no
}

kind_up
e2e_on_teardown kind_down
kind_metrics_server

log "the shipped read grant is broad, and does not include Secrets"
# RBAC has no deny rule, so this is the assertion that keeps a wildcard out.
for verb in get list watch; do
  [[ "$(can_i "$verb" secrets -A)" == "no" ]] || fail "gateway SA can $verb secrets"
done
for r in pods deployments.apps statefulsets.apps daemonsets.apps jobs.batch cronjobs.batch \
         configmaps services networkpolicies.networking.k8s.io ingresses.networking.k8s.io \
         events events.events.k8s.io pods.metrics.k8s.io nodes.metrics.k8s.io; do
  [[ "$(can_i list "$r" -A)" == "yes" ]] || fail "gateway SA cannot list $r"
done
for r in nodes persistentvolumes storageclasses.storage.k8s.io \
         customresourcedefinitions.apiextensions.k8s.io clusterroles.rbac.authorization.k8s.io; do
  [[ "$(can_i list "$r")" == "yes" ]] || fail "gateway SA cannot list $r"
done
# Not served by this cluster; granted anyway, for the adapter installed later.
for gr in external.metrics.k8s.io/queue_depth custom.metrics.k8s.io/pods; do
  [[ "$(can_i_group list "${gr%%/*}" "${gr#*/}")" == "yes" ]] || fail "gateway SA cannot list $gr"
done
# The helper must be able to say no, or the loop above proves nothing.
[[ "$(can_i_group list "" secrets)" == "no" ]] || fail "can_i_group cannot say no: it allowed secrets"
# Reads are broad; writes are not.
[[ "$(can_i delete pods -A)" == "no" ]] || fail "gateway SA can delete pods"

log "a custom resource is granted by labelling a ClusterRole, not by editing ours"
# A group nothing else grants; RBAC evaluates names, so it need not exist.
kubectl_e2e apply -f - >/dev/null <<'YAML' || fail "apply the probe ClusterRole"
apiVersion: rbac.authorization.k8s.io/v1
kind: ClusterRole
metadata:
  name: axiom-e2e-probe-read
  labels:
    axiom.dhilipkumars.github.io/aggregate-to-gateway: "true"
rules:
  - apiGroups: ["probe.axiom.test"]
    resources: ["*"]
    verbs: ["get", "list", "watch"]
YAML
probe_role_down() { kubectl_e2e delete clusterrole axiom-e2e-probe-read --ignore-not-found >/dev/null 2>&1 || true; }
e2e_on_teardown probe_role_down
deadline=$((SECONDS + 60))
until [[ "$(can_i_group list probe.axiom.test gadgets)" == "yes" ]]; do
  (( SECONDS < deadline )) || fail "a labelled ClusterRole was not aggregated into the read grant"
  sleep 1
done
probe_role_down
stack_up
# metrics.k8s.io names its resources `pods` and `nodes`, the same as the core
# group, so both of each pair are served: the gate asserts that each keeps a
# name of its own (#80) and that the short name `pods` still means core pods.
kind_deploy_gateway "pods,nodes,events,events.events.k8s.io,pods.metrics.k8s.io,nodes.metrics.k8s.io,deployments.apps,replicasets.apps"

log "applying fixture pods and a deployment, and waiting for Ready"
kind_apply "$here/fixtures/pods.yaml"
kind_apply "$here/fixtures/lean-deployment.yaml"
lean_down() { kubectl_e2e delete -f "$here/fixtures/lean-deployment.yaml" --ignore-not-found --wait=false >/dev/null 2>&1 || true; }
e2e_on_teardown lean_down
kubectl_e2e -n "$NS" rollout status deploy/lean --timeout=120s >/dev/null || fail "deployment lean did not roll out"
kind_wait_pods "$NS"

log "defining the server and importing"
psql_axiom "CREATE EXTENSION IF NOT EXISTS axiom;"
psql_axiom "DROP SERVER IF EXISTS kind CASCADE;"
psql_axiom "CREATE SERVER kind FOREIGN DATA WRAPPER axiom_fdw OPTIONS (endpoint '$E2E_GATEWAY_ENDPOINT', ca_cert '/certs/ca.crt', rpc_timeout_secs '15');"
psql_axiom "CREATE SCHEMA IF NOT EXISTS k8s;"
psql_axiom "IMPORT FOREIGN SCHEMA k8s FROM SERVER kind INTO k8s;"

log "same-plural kinds each keep a name of their own"
# #80: installing metrics-server used to rename core `pods` to `core_pods`.
# Every table is now named for its group, whatever else is served.
got="$(psql_axiom "SELECT string_agg(foreign_table_name, ',' ORDER BY foreign_table_name)
                     FROM information_schema.foreign_tables
                    WHERE foreign_table_schema = 'k8s'
                      AND foreign_table_name IN ('core_pods', 'metrics_k8s_io_pods',
                                                 'core_nodes', 'metrics_k8s_io_nodes',
                                                 'core_events', 'events_k8s_io_events');")"
[[ "$got" == "core_events,core_nodes,core_pods,events_k8s_io_events,metrics_k8s_io_nodes,metrics_k8s_io_pods" ]] \
  || fail "expected each same-plural kind under its own group-qualified name; got '$got'"

log "the short name pods means core pods, with metrics-server installed"
got="$(psql_axiom "SELECT string_agg(short_name || '>' || coalesce(target, '-'), ',' ORDER BY short_name)
                     FROM axiom_create_short_names('k8s');")"
for want in 'pods>core_pods' 'nodes>core_nodes' 'events>core_events'; do
  [[ ",$got," == *",$want,"* ]] || fail "short names should include $want; got '$got'"
done
echo "$got"

log "metrics reach SQL through the gateway ServiceAccount, and cover the fixture pods"
# The fixture pods are seconds old, and metrics-server reports a pod only after
# it has scraped it -- until then `kubectl top` exits non-zero, which under
# pipefail ended this gate with no message. Wait for every fixture pod to be
# reported, then compare; SQL is re-read each time since it trails the same way.
pods="$(kubectl_e2e get pods -n "$NS" --no-headers -o custom-columns=:metadata.name | sort | tr '\n' ',')"
deadline=$((SECONDS + 180))
while :; do
  want="$(kubectl_e2e top pods -n "$NS" --no-headers 2>/dev/null | awk '{print $1}' | sort | tr '\n' ',' || true)"
  got="$(psql_axiom "SELECT coalesce(string_agg(name, ',' ORDER BY name) || ',', '')
                       FROM k8s.metrics_k8s_io_pods WHERE namespace = '$NS';")"
  [[ "$want" == "$pods" && "$got" == "$want" ]] && break
  (( SECONDS < deadline )) || fail $'pod metrics never covered the fixture pods\n--- pods:     '"$pods"$'\n--- kubectl:  '"$want"$'\n--- postgres: '"$got"
  sleep 5
done
echo "$got"

log "node metrics are present too"
got="$(psql_axiom "SELECT count(*) FROM k8s.metrics_k8s_io_nodes;")"
[[ "$got" -ge 1 ]] || fail "expected at least one node in metrics_k8s_io_nodes, got '$got'"

log "axiom_quantity turns quantity strings into numbers that compare and sum"
# Exactness, not approximation: 100m is 0.1, and Mi is 1024-based, not 1000.
got="$(psql_axiom "SELECT axiom_quantity('100m')::text || '|' || axiom_quantity('128Mi')::text || '|' || axiom_quantity('49903n')::text;")"
[[ "$got" == "0.100|134217728|0.000049903" ]] \
  || fail "axiom_quantity conversions wrong: '$got'"
# NULL rather than an error, so one malformed field cannot fail a cluster-wide query.
got="$(psql_axiom "SELECT coalesce(axiom_quantity('nonsense')::text, 'NULL');")"
[[ "$got" == "NULL" ]] || fail "axiom_quantity('nonsense') should be NULL, got '$got'"

log "the numbers are usable: real memory usage summed across the fixture pods"
got="$(psql_axiom "SELECT sum(axiom_quantity(c->'usage'->>'memory')) > 0
                     FROM k8s.metrics_k8s_io_pods m, jsonb_array_elements(m.containers) c
                    WHERE m.namespace = '$NS';")"
[[ "$got" == "t" ]] || fail "summed memory usage was not positive: '$got'"

log "events reach SQL from both API groups"
for t in core_events events_k8s_io_events; do
  psql_axiom "SELECT 1 FROM k8s.$t LIMIT 1;" >/dev/null || fail "$t is not queryable"
done

log "a failing pod and the warning that explains it, on one row"
# The documented "why is that pod not running?" query, verbatim in shape: event
# fields are scalar jsonb, unwrapped with #>> '{}'. An image that cannot resolve
# fails fast and produces a Warning without depending on any registry.
kubectl_e2e -n "$NS" run broken --image=registry.invalid/axiom/nope:1 --restart=Never >/dev/null \
  || fail "create the broken pod"
broken_down() { kubectl_e2e -n "$NS" delete pod broken --ignore-not-found --wait=false >/dev/null 2>&1 || true; }
e2e_on_teardown broken_down
deadline=$((SECONDS + 90))
while :; do
  got="$(psql_axiom "
    SELECT p.name || '|' || (e.reason #>> '{}')
      FROM k8s.core_events e
      JOIN k8s.core_pods p ON p.namespace = e.namespace
                          AND p.name = e.involved_object->>'name'
     WHERE e.type #>> '{}' = 'Warning'
       AND e.involved_object->>'kind' = 'Pod'
       AND p.namespace = '$NS' AND p.name = 'broken'
     LIMIT 1;")"
  [[ -n "$got" ]] && break
  (( SECONDS < deadline )) || fail "no Warning event joined to the broken pod within 90s"
  sleep 3
done
echo "$got"
[[ "$got" == broken\|* && "$got" != "broken|" ]] || fail "warning row has no reason: '$got'"

log "the differentiating query runs: usage joined to live spec in one statement"
psql_axiom "
  WITH usage AS (
    SELECT m.namespace, m.name AS pod,
           sum(axiom_quantity(c->'usage'->>'memory')) AS mem_used
      FROM k8s.metrics_k8s_io_pods m, jsonb_array_elements(m.containers) c
     GROUP BY 1,2)
  SELECT count(*)
    FROM k8s.core_pods p
    LEFT JOIN usage u ON u.namespace = p.namespace AND u.pod = p.name
   WHERE p.namespace = '$NS';" >/dev/null || fail "the cross-source join failed"

log "find and fix in one statement: annotate deployments using under 20% of the memory they request"
# The README's query, verbatim apart from the namespace filter. It reads live
# usage and live spec, walks pod -> ReplicaSet -> Deployment, and writes the
# answer back as a real Kubernetes update, which no read-only tool can do.
got="$(psql_axiom "
WITH used AS (
  SELECT m.namespace, m.name AS pod, sum(axiom_quantity(c->'usage'->>'memory')) AS bytes
    FROM k8s.metrics_k8s_io_pods m, jsonb_array_elements(m.containers) c
   GROUP BY 1, 2),
requested AS (
  SELECT p.namespace, p.name AS pod,
         r.metadata->'ownerReferences'->0->>'name' AS deployment,
         sum(axiom_quantity(c->'resources'->'requests'->>'memory')) AS bytes
    FROM k8s.core_pods p
    JOIN k8s.apps_replicasets r
      ON r.namespace = p.namespace AND r.name = p.metadata->'ownerReferences'->0->>'name',
         jsonb_array_elements(p.spec->'containers') c
   GROUP BY 1, 2, 3),
ratio AS (
  SELECT q.namespace, q.deployment, round(100 * sum(u.bytes) / sum(q.bytes)) AS pct
    FROM requested q JOIN used u USING (namespace, pod)
   GROUP BY 1, 2
  HAVING sum(q.bytes) > 0)
UPDATE k8s.apps_deployments d
   SET annotations = coalesce(d.annotations, '{}')
                     || jsonb_build_object('axiom/memory-used-pct', r.pct::text)
  FROM ratio r
 WHERE d.namespace = r.namespace AND d.name = r.deployment AND r.pct < 20
   AND d.namespace = '$NS'
RETURNING d.name || '|' || r.pct;")" || fail "the find-and-fix UPDATE failed"
echo "$got"
[[ "$got" == lean\|* ]] || fail "expected the UPDATE to annotate lean; RETURNING gave '$got'"
pct="${got#lean|}"
annotation="$(kubectl_e2e -n "$NS" get deploy lean -o jsonpath='{.metadata.annotations.axiom/memory-used-pct}')"
[[ "$annotation" == "$pct" ]] \
  || fail "the annotation on deploy/lean is '$annotation', want '$pct' from RETURNING"
echo "deploy/lean annotated axiom/memory-used-pct=$annotation"

log "the cluster joins to the application's own tables"
# Axiom lives in the application's Postgres, so live cluster state joins to
# business data in one query, with no export and no copy that goes stale.
psql_axiom "DROP TABLE IF EXISTS tenants;
            CREATE TABLE tenants (namespace text PRIMARY KEY, customer text, plan text);
            INSERT INTO tenants VALUES ('$NS', 'Acme', 'enterprise');" >/dev/null
got="$(psql_axiom "
SELECT t.customer || '|' || t.plan || '|' || p.name || '|' || (e.reason #>> '{}')
  FROM tenants t
  JOIN k8s.core_pods p ON p.namespace = t.namespace
  JOIN k8s.core_events e ON e.namespace = p.namespace
                        AND e.involved_object->>'name' = p.name
 WHERE e.type #>> '{}' = 'Warning'
   AND e.involved_object->>'kind' = 'Pod'
   AND p.name = 'broken'
 LIMIT 1;")"
[[ "$got" == "Acme|enterprise|broken|"?* ]] || fail "customer join to failing pods gave '$got'"
echo "$got"
got="$(psql_axiom "
SELECT t.customer || '|' || (sum(axiom_quantity(c->'usage'->>'memory')) > 0)
  FROM tenants t
  JOIN k8s.metrics_k8s_io_pods m ON m.namespace = t.namespace,
       jsonb_array_elements(m.containers) c
 GROUP BY t.customer;")"
[[ "$got" == "Acme|t" ]] || fail "memory per customer gave '$got'"
psql_axiom "DROP TABLE tenants;" >/dev/null

log "PASS"
