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
#   E2E_GATEWAY_LOCAL_IMAGE   locally built image to side-load (default axiom-gateway:latest)
#   E2E_GATEWAY_DEPLOY_IMAGE  image reference the manifest uses (default ghcr.io/dhilipkumars/axiom-gateway:latest)
#   E2E_GATEWAY_PULL_POLICY   pull policy E2E patches onto the Deployment (default IfNotPresent)

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
  # 0755: the gateway container runs as uid 65532 and must traverse this
  # directory through the bind mount on Linux. Only the gateway kubeconfig is
  # world-readable; the admin kubeconfig stays 0600.
  mkdir -p "$E2E_KUBE_DIR" && chmod 0755 "$E2E_KUBE_DIR"
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
  # inside a gitignored directory; only this file is mounted into the container.
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

# --- in-cluster gateway (Phase 6) -------------------------------------------
#
# The gateway used to run as a compose container holding a kubeconfig, so
# rest.InClusterConfig() never executed and no Deployment manifest was ever
# exercised. These helpers run it the way docs/DESIGN.md §5.1 describes: a
# Deployment in the cluster it manages, reached from outside over a NodePort.
#
# Postgres deliberately stays outside. Axiom exists so a database that cannot
# join the cluster network can still query it; moving Postgres in would hide the
# routing and TLS-boundary failures the design is meant to survive.

# NodePort the gateway Service publishes, and the host:port Postgres dials. The
# compose stack joins kind's Docker network, so it reaches the node by name.
E2E_GATEWAY_NODEPORT="${E2E_GATEWAY_NODEPORT:-30443}"
E2E_GATEWAY_LOCAL_IMAGE="${E2E_GATEWAY_LOCAL_IMAGE:-axiom-gateway:latest}"
# Must match the image in deploy/k8s/gateway-deployment.yaml: the suite
# side-loads its own build under this tag rather than editing the manifest,
# so a mismatch means the Pod pulls a published image instead of the one
# under test -- and the gates would then pass against the wrong binary.
E2E_GATEWAY_DEPLOY_IMAGE="${E2E_GATEWAY_DEPLOY_IMAGE:-ghcr.io/dhilipkumars/axiom-gateway:latest}"
E2E_GATEWAY_PULL_POLICY="${E2E_GATEWAY_PULL_POLICY:-IfNotPresent}"

# kind_gateway_endpoint: the https URL an out-of-cluster client uses.
kind_gateway_endpoint() {
  echo "https://${E2E_KIND_CLUSTER}-control-plane:${E2E_GATEWAY_NODEPORT}"
}

# kind_load_gateway_image: side-load the locally built image into the cluster,
# retagged to match the raw manifest's published-image reference. Done once per
# suite rather than per gate: each load costs 20-40s and the image does not
# change between gates.
kind_load_gateway_image() {
  local source="${1:-$E2E_GATEWAY_LOCAL_IMAGE}" image="${2:-$E2E_GATEWAY_DEPLOY_IMAGE}"
  local node="${E2E_KIND_CLUSTER}-control-plane"
  local stamp="/etc/axiom-loaded-image-id"
  docker image inspect "$source" >/dev/null 2>&1 \
    || fail "image $source is not built; run 'docker compose -f $E2E_COMPOSE_FILE build gateway' first"

  # Skip when the node already has this exact build. Compare by the *docker*
  # image ID recorded at load time, not by anything containerd reports:
  # `kind load` re-digests the image on the way in, so the containerd image ID
  # is never equal to the docker one and a comparison between them can only
  # ever say "absent". That is what the previous version of this check did,
  # which meant every gate reloaded and the skip was decorative.
  #
  # A stamp file on the node says exactly what is wanted -- "this node holds
  # the image built from this docker image ID" -- and a rebuilt image changes
  # the ID, so a stale binary can never be served.
  #
  # `|| true` on the substitutions: under `set -e` a command substitution that
  # exits non-zero aborts the assignment and takes the whole gate with it, with
  # no error message, and both of these legitimately fail on a first run.
  local source_id deploy_id want have restore="" created_ref=0
  source_id="$(docker image inspect "$source" --format '{{.Id}}' 2>/dev/null || true)"
  deploy_id="$(docker image inspect "$image" --format '{{.Id}}' 2>/dev/null || true)"
  want="${image}|${source_id}"
  have="$(docker exec "$node" cat "$stamp" 2>/dev/null || true)"
  if [[ -n "$want" && "$want" == "$have" ]]; then
    log "$source already loaded in $E2E_KIND_CLUSTER as $image, skipping"
    return 0
  fi

  if [[ "$source" != "$image" && "$deploy_id" != "$source_id" ]]; then
    if [[ -n "$deploy_id" ]]; then
      restore="axiom-e2e-restore:$RANDOM-$$"
      docker tag "$image" "$restore" >/dev/null || fail "save existing $image tag"
    else
      created_ref=1
    fi
    docker tag "$source" "$image" >/dev/null || fail "tag $source as $image"
  fi
  log "loading $source into kind cluster $E2E_KIND_CLUSTER as $image"
  kind load docker-image "$image" --name "$E2E_KIND_CLUSTER" >/dev/null \
    || fail "kind load docker-image $image"
  # Written only after a successful load, so an interrupted one reloads.
  docker exec "$node" sh -c "printf '%s' '$want' > $stamp" >/dev/null 2>&1 || true
  # The deploy-image tag is just a staging name for `kind load`; restore an
  # existing local reference or remove only the one this run created.
  if [[ -n "$restore" ]]; then
    docker tag "$restore" "$image" >/dev/null || fail "restore existing $image tag"
    docker image rm "$restore" >/dev/null 2>&1 || true
  elif (( created_ref )); then
    docker image rm "$image" >/dev/null 2>&1 || true
  fi
}

# kind_gateway_tls_secret: publish the compose-generated CA and server cert as a
# Secret. The same material is used on both sides, so Postgres keeps trusting
# the gateway across the move; the certificate's SANs already cover the
# in-cluster Service names and the kind node (deploy/compose/certs/gen.sh).
kind_gateway_tls_secret() {
  local vol="axiom_certs" tmp
  tmp="$(mktemp -d)"
  # The certs live in a Docker volume, not on the host; copy them out through a
  # throwaway container rather than duplicating the generation logic.
  #
  # The copy is chowned to the invoking user because gen.sh leaves the key as
  # mode 0600 owned by uid 65532 (the distroless `nonroot` the gateway runs as)
  # and busybox `cp` carries those bits onto the copy. On Linux that makes the
  # copy unreadable to whoever runs the gate -- kubectl fails with "permission
  # denied" and the secret is never created. Docker Desktop's file sharing
  # remaps ownership on the way out, which is why this only ever failed in CI.
  #
  # Chowning rather than chmodding: the key stays 0600, just owned by the user
  # who needs it, so a private key is never briefly world-readable on disk.
  docker run --rm -v "$vol":/certs:ro -v "$tmp":/out alpine:3.20 \
    sh -c "cp /certs/gateway.crt /certs/gateway.key /certs/ca.crt /out/ &&
           chown $(id -u):$(id -g) /out/gateway.crt /out/gateway.key /out/ca.crt &&
           chmod 0600 /out/gateway.key" >/dev/null 2>&1 \
    || fail "could not read certificates from volume $vol (is the stack up?)"
  # Fail here rather than letting kubectl report it: a secret built from an
  # unreadable file is the failure this guard exists for.
  [[ -r "$tmp/gateway.key" && -r "$tmp/gateway.crt" ]] \
    || fail "copied certificates are not readable as $(id -un) (uid $(id -u)); \
check ownership in $tmp"
  kubectl_e2e -n "$E2E_GATEWAY_SA_NS" create secret generic axiom-gateway-tls \
    --from-file=tls.crt="$tmp/gateway.crt" \
    --from-file=tls.key="$tmp/gateway.key" \
    --dry-run=client -o yaml | kubectl_e2e apply -f - >/dev/null \
    || fail "create secret axiom-gateway-tls"
  rm -rf "$tmp"
}

# kind_deploy_gateway [SERVE]: apply the Deployment and wait for it to be ready.
# SERVE is the --serve allowlist; gates need different values, so it is applied
# with `kubectl set env` after the manifest rather than baked into it.
kind_deploy_gateway() {
  local serve="${1:-pods,configmaps,widgets.example.com}"
  # Second argument: how long the gateway trusts a cached resource list.
  # Seconds here, because a gate cannot wait out the five-minute default to
  # prove that a deleted kind stops being offered.
  local discovery_ttl="${2:-${E2E_DISCOVERY_TTL:-5m}}"
  # A standalone gate has no suite to have loaded the image for it, and the
  # load is skipped when the node already has it, so this is safe either way.
  kind_load_gateway_image
  kubectl_e2e apply -f "$E2E_ROOT/deploy/k8s/gateway-rbac.yaml" >/dev/null || fail "apply RBAC"
  kind_gateway_tls_secret
  log "deploying the gateway in-cluster (serve=$serve, discovery-ttl=$discovery_ttl)"
  # The checked-in manifest uses Always because :development is a moving tag.
  # E2E needs the opposite: a cache-preferring policy so the side-loaded local
  # build wins without a registry pull. Patch the manifest *before* apply so
  # the first Pod is created with the override already in place.
  sed "0,/imagePullPolicy: Always/s//imagePullPolicy: $E2E_GATEWAY_PULL_POLICY/" \
    "$E2E_ROOT/deploy/k8s/gateway-deployment.yaml" | kubectl_e2e apply -f - >/dev/null \
    || fail "apply gateway deployment"
  # Override the manifest's defaults the same way an operator would. Not
  # ConfigMap keys: a referenced key is required, and a Pod whose ConfigMap
  # lacks it never starts (deploy/k8s/gateway-deployment.yaml says why).
  # Each gate needs its own serve list, which is why this is set here rather
  # than baked into the manifest.
  kubectl_e2e -n "$E2E_GATEWAY_SA_NS" set env deploy/axiom-gateway \
    AXIOM_SERVE="$serve" AXIOM_DISCOVERY_TTL="$discovery_ttl" >/dev/null \
    || fail "set AXIOM_SERVE / AXIOM_DISCOVERY_TTL"
  # `set env` on an unchanged value patches nothing, so force a fresh Pod.
  # Gates also need a clean process: the gateway caches discovery and access
  # answers for its lifetime, and those must not leak between gates.
  kubectl_e2e -n "$E2E_GATEWAY_SA_NS" rollout restart deploy/axiom-gateway >/dev/null 2>&1 || true
  kubectl_e2e -n "$E2E_GATEWAY_SA_NS" rollout status deploy/axiom-gateway --timeout=120s >/dev/null \
    || { kubectl_e2e -n "$E2E_GATEWAY_SA_NS" describe pod -l app.kubernetes.io/name=axiom-gateway | tail -30
         fail "gateway deployment did not become ready"; }
  kind_wait_gateway_endpoint
}

# kind_wait_gateway_endpoint: block until the Service has exactly one ready
# endpoint. `rollout status` returning is not enough on its own: the readiness
# probe is a TCP check, so it passes the moment the listener is up, and during a
# replacement the Service can still carry the outgoing Pod. A caller that
# proceeds then sees its first RPC hang until its deadline.
kind_wait_gateway_endpoint() {
  local deadline=$((SECONDS + 90)) n
  while :; do
    n="$(kubectl_e2e -n "$E2E_GATEWAY_SA_NS" get endpointslice \
           -l kubernetes.io/service-name=axiom-gateway \
           -o jsonpath='{range .items[*]}{range .endpoints[?(@.conditions.ready==true)]}{.addresses[0]}{"\n"}{end}{end}' 2>/dev/null \
         | grep -c . || true)"
    [[ "$n" == "1" ]] && break
    (( SECONDS < deadline )) || fail "gateway Service has $n ready endpoints, want exactly 1"
    sleep 1
  done
  kind_wait_gateway_nodeport
}

# kind_wait_gateway_nodeport: block until the NodePort actually accepts a
# connection from the Docker network Postgres is on.
#
# A ready endpoint is not the same as a reachable NodePort. kube-proxy programs
# the node's forwarding rules asynchronously after the EndpointSlice changes,
# and until it has, a connection to the node port is refused. Under `Recreate`
# there is no old Pod to carry the traffic in the meantime, so the window is a
# real outage rather than a brief inconsistency.
#
# That window is short enough to miss on a fast machine and long enough to lose
# on a loaded CI runner, which is exactly how it showed up: every gate passing
# locally, and the cluster gate failing in CI with `Unavailable: tcp connect
# error` on the first query after a restart.
#
# The probe runs from a throwaway container on kind's network because that is
# the path Postgres takes. Probing from the host would test a different route,
# and probing from inside the cluster would not test the NodePort at all.
kind_wait_gateway_nodeport() {
  local node="${E2E_KIND_CLUSTER}-control-plane" port="$E2E_GATEWAY_NODEPORT"
  # One container with an internal retry loop, rather than one per attempt:
  # `docker run` costs more than the wait usually does.
  docker run --rm --network kind alpine:3.20 sh -c \
    "for i in \$(seq 1 60); do nc -z -w 2 $node $port && exit 0; sleep 1; done; exit 1" \
    >/dev/null 2>&1 \
    || fail "gateway NodePort $node:$port did not accept connections within 60s"
}

# kind_gateway_logs: the in-cluster gateway's logs, for assertions that inspect
# a specific call rather than counting. Counting assertions use
# axiom_gateway_stats() instead, because a Pod restart starts a fresh log.
kind_gateway_logs() {
  kubectl_e2e -n "$E2E_GATEWAY_SA_NS" logs deploy/axiom-gateway --tail=-1 2>/dev/null || true
}

# kind_restart_gateway: replace the gateway Pod and wait. The in-cluster
# equivalent of `compose restart gateway`.
kind_restart_gateway() {
  kubectl_e2e -n "$E2E_GATEWAY_SA_NS" rollout restart deploy/axiom-gateway >/dev/null \
    || fail "rollout restart"
  kubectl_e2e -n "$E2E_GATEWAY_SA_NS" rollout status deploy/axiom-gateway --timeout=120s >/dev/null \
    || fail "gateway did not come back after restart"
  kind_wait_gateway_endpoint
}

# kind_stop_gateway / kind_start_gateway: take the gateway away and bring it
# back, for the degraded-watch assertions.
kind_stop_gateway() {
  kubectl_e2e -n "$E2E_GATEWAY_SA_NS" scale deploy/axiom-gateway --replicas=0 >/dev/null || fail "scale to 0"
  kubectl_e2e -n "$E2E_GATEWAY_SA_NS" wait --for=delete pod -l app.kubernetes.io/name=axiom-gateway --timeout=60s >/dev/null 2>&1 || true
}
kind_start_gateway() {
  kubectl_e2e -n "$E2E_GATEWAY_SA_NS" scale deploy/axiom-gateway --replicas=1 >/dev/null || fail "scale to 1"
  kubectl_e2e -n "$E2E_GATEWAY_SA_NS" rollout status deploy/axiom-gateway --timeout=120s >/dev/null \
    || fail "gateway did not come back"
  kind_wait_gateway_endpoint
}
