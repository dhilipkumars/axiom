#!/usr/bin/env bash
# Reusable E2E setup for a local kind cluster plus a least-privilege gateway
# identity. Source after lib/stack.sh; do not execute directly.
#
#   kind_up                 # create (or reuse) the cluster, apply RBAC, write kubeconfig
#   kind_apply FILE         # kubectl apply -f
#   kind_wait_pods NS       # wait until all pods in NS are Ready
#   kubectl_e2e ARGS...     # kubectl against the cluster as admin (test oracle)
#
# Environment knobs (all optional):
#   E2E_KIND_CLUSTER   cluster name (default axiom-e2e)
#   E2E_KIND_KEEP=1    leave the cluster running after the test
#   E2E_KUBE_DIR       where the gateway kubeconfig is written (default e2e/.kind)

[[ -n "${_AXIOM_E2E_KIND_LIB:-}" ]] && return 0
_AXIOM_E2E_KIND_LIB=1

: "${E2E_ROOT:?source lib/stack.sh first}"
E2E_KIND_CLUSTER="${E2E_KIND_CLUSTER:-axiom-e2e}"
E2E_KIND_KEEP="${E2E_KIND_KEEP:-0}"
export E2E_KUBE_DIR="${E2E_KUBE_DIR:-$E2E_ROOT/e2e/.kind}"
E2E_ADMIN_KUBECONFIG="$E2E_KUBE_DIR/admin"
E2E_GATEWAY_KUBECONFIG="$E2E_KUBE_DIR/config"
E2E_GATEWAY_SA_NS="axiom-system"
E2E_GATEWAY_SA="axiom-gateway"

# kubectl_e2e: admin kubectl against the E2E cluster (host-side API address).
kubectl_e2e() { kubectl --kubeconfig "$E2E_ADMIN_KUBECONFIG" "$@"; }

kind_down() {
  if [[ "$E2E_KIND_KEEP" == "1" ]]; then log "E2E_KIND_KEEP=1, leaving kind cluster $E2E_KIND_CLUSTER"; return 0; fi
  log "deleting kind cluster $E2E_KIND_CLUSTER"
  kind delete cluster --name "$E2E_KIND_CLUSTER" >/dev/null 2>&1 || true
  rm -rf "$E2E_KUBE_DIR"
}

# kind_up: ensure the cluster exists, apply deploy/k8s/gateway-rbac.yaml, and
# write $E2E_GATEWAY_KUBECONFIG: the cluster CA + internal API address (reachable
# from the compose network) + a 1h token for the gateway ServiceAccount. The
# token grants only what the ClusterRole allows (pods get/list).
kind_up() {
  command -v kind >/dev/null || fail "kind is not installed"
  command -v kubectl >/dev/null || fail "kubectl is not installed"
  mkdir -p "$E2E_KUBE_DIR" && chmod 0700 "$E2E_KUBE_DIR"
  if ! kind get clusters 2>/dev/null | grep -qx "$E2E_KIND_CLUSTER"; then
    log "creating kind cluster $E2E_KIND_CLUSTER"
    kind create cluster --name "$E2E_KIND_CLUSTER" --wait 120s >/dev/null || fail "kind create cluster failed"
  else
    log "reusing kind cluster $E2E_KIND_CLUSTER"
  fi
  kind get kubeconfig --name "$E2E_KIND_CLUSTER" > "$E2E_ADMIN_KUBECONFIG"
  chmod 0600 "$E2E_ADMIN_KUBECONFIG"

  log "applying least-privilege gateway RBAC"
  kubectl_e2e apply -f "$E2E_ROOT/deploy/k8s/gateway-rbac.yaml" >/dev/null || fail "apply RBAC"

  log "writing gateway kubeconfig (SA token, internal API address)"
  local ca="$E2E_KUBE_DIR/cluster-ca.crt" token
  kind get kubeconfig --name "$E2E_KIND_CLUSTER" --internal \
    | kubectl config view --kubeconfig /dev/stdin --raw -o jsonpath='{.clusters[0].cluster.certificate-authority-data}' \
    | base64 -d > "$ca" || fail "extract cluster CA"
  token="$(kubectl_e2e -n "$E2E_GATEWAY_SA_NS" create token "$E2E_GATEWAY_SA" --duration=1h)" || fail "create SA token"
  rm -f "$E2E_GATEWAY_KUBECONFIG"
  kubectl --kubeconfig "$E2E_GATEWAY_KUBECONFIG" config set-cluster kind \
    --server="https://${E2E_KIND_CLUSTER}-control-plane:6443" --certificate-authority="$ca" --embed-certs=true >/dev/null
  kubectl --kubeconfig "$E2E_GATEWAY_KUBECONFIG" config set-credentials gateway --token="$token" >/dev/null
  kubectl --kubeconfig "$E2E_GATEWAY_KUBECONFIG" config set-context kind --cluster=kind --user=gateway >/dev/null
  kubectl --kubeconfig "$E2E_GATEWAY_KUBECONFIG" config use-context kind >/dev/null
  # Readable by the gateway container's non-root uid via bind mount. Test-only:
  # a 1h token for a pods-read-only ServiceAccount in a disposable cluster,
  # inside a 0700 directory that is gitignored.
  chmod 0644 "$E2E_GATEWAY_KUBECONFIG"
  rm -f "$ca"
  unset token
}

kind_apply() { kubectl_e2e apply -f "$1" >/dev/null || fail "kubectl apply -f $1"; }

# kind_wait_pods NS [TIMEOUT]: wait for every pod in NS to be Ready.
kind_wait_pods() {
  local ns="$1" timeout="${2:-120}s"
  kubectl_e2e -n "$ns" wait --for=condition=Ready pod --all --timeout="$timeout" >/dev/null \
    || fail "pods in $ns did not become Ready within $timeout"
}
