#!/usr/bin/env bash
# Axiom quick start: a local kind cluster with the gateway in it, Postgres with
# Axiom in Docker, connected and imported, and a first query.
#
#   bash quickstart.sh        bring it up (safe to re-run)
#   bash quickstart.sh down   remove what it created
#
#   AXIOM_PG       Postgres major: 16, 17 or 18      (default 17)
#   AXIOM_CLUSTER  kind cluster name                 (default axiom-quickstart)
#   AXIOM_PORT     psql port on 127.0.0.1, 0 = none  (default 55432)
#
# Each release attaches its own copy, with AXIOM_VERSION filled in, so the
# script installs that release's images and manifests and nothing from main.
# https://dhilipkumars.github.io/axiom/guides/quick-start/
#
# Written for the bash 3.2 macOS ships, which is what `curl ... | bash` runs
# there: no associative arrays, no mapfile, no ${x,,}.
set -euo pipefail

VERSION="${AXIOM_VERSION:-@AXIOM_VERSION@}"
PG="${AXIOM_PG:-17}"
CLUSTER="${AXIOM_CLUSTER:-axiom-quickstart}"
PORT="${AXIOM_PORT:-55432}"
STATE="${AXIOM_STATE_DIR:-$HOME/.axiom-quickstart}"
# For testing unreleased builds; a user never needs these.
PG_IMAGE="${AXIOM_PG_IMAGE:-ghcr.io/dhilipkumars/axiom-postgres:${VERSION}-pg${PG}}"
GW_IMAGE="${AXIOM_GATEWAY_IMAGE:-ghcr.io/dhilipkumars/axiom-gateway:v${VERSION}}"
MANIFESTS="${AXIOM_MANIFESTS:-https://raw.githubusercontent.com/dhilipkumars/axiom/v${VERSION}/deploy/k8s}"

# Named for the cluster, so two runs with different names never touch each
# other's Postgres.
CONTAINER="${CLUSTER}-pg"
NODE="${CLUSTER}-control-plane"
CTX="kind-${CLUSTER}"
# What this script made, so `down` and a re-run never touch anything it did
# not: the state directory carries SENTINEL, the cluster is recorded in
# MARKER_NAME, and the container carries LABEL.
SENTINEL=.axiom-quickstart
MARKER_NAME=created-cluster
LABEL=io.github.dhilipkumars.axiom.quickstart

# $STATE as a person would write it, ~ for the home directory.
shown() { printf '%s' "${STATE/#$HOME/~}"; }
say() { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
die() { printf '\n\033[31merror:\033[0m %s\n' "$*" >&2; exit 1; }
k()   { kubectl --context "$CTX" "$@"; }
psql_pg() { docker exec -i "$CONTAINER" psql -U postgres -v ON_ERROR_STOP=1 "$@"; }

preflight() {
  case "$VERSION" in
    @*) die "this copy has no version filled in. Download quickstart.sh from a release:
       https://github.com/dhilipkumars/axiom/releases/latest" ;;
  esac
  case "$PG" in 16|17|18) ;; *) die "AXIOM_PG must be 16, 17 or 18, not '$PG'" ;; esac
  local t
  for t in docker kind kubectl; do
    command -v "$t" >/dev/null 2>&1 || die "$t is not installed; see https://dhilipkumars.github.io/axiom/guides/prerequisites/"
  done
  case "$MANIFESTS" in
    http*) command -v curl >/dev/null 2>&1 || die "curl is not installed" ;;
  esac
  docker info >/dev/null 2>&1 || die "Docker is not running"
}

cluster_exists() { kind get clusters 2>/dev/null | grep -qx "$CLUSTER"; }
we_created_it()  { [[ -f "$STATE/$MARKER_NAME" && "$(cat "$STATE/$MARKER_NAME")" == "$CLUSTER" ]]; }
# Whether a container of that name exists, and whether it is ours.
container_exists() { docker inspect "$CONTAINER" >/dev/null 2>&1; }
container_ours() {
  [[ "$(docker inspect -f "{{ index .Config.Labels \"$LABEL\" }}" "$CONTAINER" 2>/dev/null)" == "$CLUSTER" ]]
}

# Take the state directory: absolute, because Docker refuses relative mount
# paths, and only if it is empty or already this script's.
claim_state() {
  mkdir -p "$STATE"
  STATE="$(cd "$STATE" && pwd)"
  if [[ ! -f "$STATE/$SENTINEL" ]]; then
    [[ -z "$(ls -A "$STATE")" ]] || die "$STATE is not empty and was not made by this script.
       Set AXIOM_STATE_DIR to an empty or new directory."
    : > "$STATE/$SENTINEL"
  fi
}

# The gateway manifest, pinned to this release's image before it is applied,
# so the cluster never starts the floating :latest even for a moment.
gateway_manifest() {
  local src
  case "$MANIFESTS" in
    http*) src="$(curl -fsSL "$MANIFESTS/gateway-deployment.yaml")" || die "could not fetch $MANIFESTS/gateway-deployment.yaml" ;;
    *)     src="$(cat "$MANIFESTS/gateway-deployment.yaml")" ;;
  esac
  src="$(printf '%s\n' "$src" | sed "s|image: ghcr.io/dhilipkumars/axiom-gateway:.*|image: $GW_IMAGE|")"
  printf '%s\n' "$src" | grep -q "image: $GW_IMAGE\$" || die "could not pin the gateway image in the manifest"
  printf '%s\n' "$src"
}

up() {
  preflight
  claim_state
  echo "Axiom $VERSION, Postgres $PG, kind cluster '$CLUSTER'"

  say "kind cluster"
  if cluster_exists; then
    we_created_it || die "a kind cluster named '$CLUSTER' already exists, and this script did not
       create it. It would replace that cluster's gateway, so it stops here.
       Use another name (AXIOM_CLUSTER=...), or delete that cluster yourself."
    echo "reusing $CLUSTER, created by an earlier run"
  else
    # Recorded before creating, so an interrupted create is still ours to
    # clean up rather than a cluster no later run will touch.
    echo "$CLUSTER" > "$STATE/$MARKER_NAME"
    kind create cluster --quiet --name "$CLUSTER" || die "kind could not create the cluster"
    echo "created $CLUSTER"
  fi
  k wait --for=condition=Ready node --all --timeout=180s >/dev/null

  say "TLS keypair for the gateway"
  # The certificate must name the node Postgres dials, so it is made for this
  # cluster; one from a run with another cluster name is replaced.
  if [[ -f "$STATE/certs/ca.crt" && -f "$STATE/certs/gateway.crt" \
        && -f "$STATE/certs/gateway.key" && -f "$STATE/certs/cluster" \
        && "$(cat "$STATE/certs/cluster")" == "$CLUSTER" ]]; then
    echo "reusing $(shown)/certs"
  else
    rm -rf "$STATE/certs"; mkdir -p "$STATE/certs"
    # In a container, because macOS's LibreSSL makes certificates the gateway
    # rejects.
    docker run --rm -v "$STATE/certs:/certs" -w /certs \
      --entrypoint /bin/sh alpine/openssl:3.3.3 -c "
        openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
          -days 365 -subj '/CN=axiom-quickstart-ca' -keyout ca.key -out ca.crt
        openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
          -subj '/CN=gateway' -keyout gateway.key -out gateway.csr
        printf 'subjectAltName=DNS:$NODE,DNS:axiom-gateway.axiom-system.svc\nextendedKeyUsage=serverAuth\n' > san.cnf
        openssl x509 -req -in gateway.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
          -days 365 -extfile san.cnf -out gateway.crt
        rm -f gateway.csr san.cnf ca.srl ca.key
        chown $(id -u):$(id -g) ca.crt gateway.crt gateway.key
      " >/dev/null 2>&1 || die "could not generate the keypair"
    echo "$CLUSTER" > "$STATE/certs/cluster"
    echo "written to $(shown)/certs"
  fi

  say "gateway ($GW_IMAGE)"
  k apply -f "$MANIFESTS/gateway-rbac.yaml" >/dev/null
  k -n axiom-system delete secret axiom-gateway-tls --ignore-not-found >/dev/null
  k -n axiom-system create secret generic axiom-gateway-tls \
    --from-file=tls.crt="$STATE/certs/gateway.crt" \
    --from-file=tls.key="$STATE/certs/gateway.key" >/dev/null
  gateway_manifest | k apply -f - >/dev/null
  # On a re-run nothing in the Deployment changed, so without a restart the
  # running pod would keep the certificate from the previous run.
  k -n axiom-system rollout restart deploy/axiom-gateway >/dev/null
  k -n axiom-system rollout status deploy/axiom-gateway --timeout=180s >/dev/null \
    || die "the gateway did not become ready: kubectl --context $CTX -n axiom-system logs deploy/axiom-gateway"
  echo "running in namespace axiom-system, NodePort 30443"

  say "Postgres $PG with Axiom ($PG_IMAGE)"
  docker pull -q "$PG_IMAGE" >/dev/null || die "could not pull $PG_IMAGE"
  if container_exists; then
    container_ours || die "a container named $CONTAINER exists and was not made by this script"
    docker rm -f "$CONTAINER" >/dev/null
  fi
  local publish=""
  if [[ "$PORT" != "0" ]]; then
    if (exec 3<>"/dev/tcp/127.0.0.1/$PORT") 2>/dev/null; then
      die "something is already listening on 127.0.0.1:$PORT. Set AXIOM_PORT to another port, or 0 for none."
    fi
    # Loopback only: the password below is no secret.
    publish="-p 127.0.0.1:$PORT:5432"
  fi
  # shellcheck disable=SC2086  # $publish is empty or two words, on purpose
  docker run -d --name "$CONTAINER" --network kind --label "$LABEL=$CLUSTER" \
    -e POSTGRES_PASSWORD=axiom \
    -v "$STATE/certs:/certs:ro" $publish \
    "$PG_IMAGE" >/dev/null
  local _
  for _ in $(seq 1 90); do
    docker exec "$CONTAINER" pg_isready -U postgres -h 127.0.0.1 >/dev/null 2>&1 && break
    sleep 2
  done
  docker exec "$CONTAINER" pg_isready -U postgres -h 127.0.0.1 >/dev/null 2>&1 \
    || die "Postgres did not start: docker logs $CONTAINER"
  echo "running as container $CONTAINER"

  say "connect Postgres to the gateway, and import the cluster"
  psql_pg -q >/dev/null <<SQL
SET client_min_messages = warning;
CREATE EXTENSION IF NOT EXISTS axiom;
DROP SERVER IF EXISTS prod CASCADE;
CREATE SERVER prod FOREIGN DATA WRAPPER axiom_fdw
  OPTIONS (endpoint 'https://${NODE}:30443', ca_cert '/certs/ca.crt', rpc_timeout_secs '60');
CREATE USER MAPPING FOR CURRENT_USER SERVER prod;
CREATE SCHEMA IF NOT EXISTS k8s;
IMPORT FOREIGN SCHEMA k8s FROM SERVER prod INTO k8s;
SELECT * FROM axiom_create_short_names('k8s');
SQL
  psql_pg -tAc "SELECT 'axiom ' || axiom_version() || ', '
                       || count(*) || ' tables imported into schema k8s'
                  FROM information_schema.foreign_tables WHERE foreign_table_schema = 'k8s'"

  # So the first query shows a settled cluster rather than pods still starting.
  k -n kube-system wait --for=condition=Ready pod --all --timeout=120s >/dev/null 2>&1 || true

  say "a first query: the cluster's kube-system pods, from SQL"
  echo "SELECT name, phase, node FROM k8s.pods WHERE namespace = 'kube-system' ORDER BY name;"
  echo
  psql_pg -c "SELECT name, phase, node FROM k8s.pods WHERE namespace = 'kube-system' ORDER BY name"

  say "ready"
  echo "Open psql:     docker exec -it $CONTAINER psql -U postgres"
  if [[ "$PORT" != "0" ]]; then
    echo "           or: psql postgresql://postgres:axiom@127.0.0.1:$PORT/postgres"
  fi
  cat <<EOF
Examples:      https://dhilipkumars.github.io/axiom/guides/examples/
Remove it all: bash quickstart.sh down    (piped: curl ... | bash -s down)
EOF
}

down() {
  say "removing the quick start"
  if container_exists; then
    if container_ours; then
      docker rm -f "$CONTAINER" >/dev/null && echo "removed container $CONTAINER"
    else
      echo "left container $CONTAINER alone: this script did not create it"
    fi
  fi
  if [[ -d "$STATE" ]]; then STATE="$(cd "$STATE" && pwd)"; fi
  if we_created_it && cluster_exists; then
    kind delete cluster --name "$CLUSTER"
  elif cluster_exists; then
    echo "left kind cluster '$CLUSTER' alone: this script did not create it"
  fi
  # Only what this script wrote, and only in a directory it marked as its own.
  if [[ -f "$STATE/$SENTINEL" ]]; then
    rm -rf "${STATE:?}/certs" "${STATE:?}/${MARKER_NAME:?}" "${STATE:?}/${SENTINEL:?}"
    rmdir "$STATE" 2>/dev/null || true
    echo "removed $(shown)"
  fi
}

case "${1:-up}" in
  up)   up ;;
  down) down ;;
  *)    die "usage: bash quickstart.sh [up|down]" ;;
esac
