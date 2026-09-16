#!/usr/bin/env bash
# Install E2E: what a stranger gets, from a clean pull of the published images.
#
# Every other gate builds from the working tree, so they prove the *code*
# works. None of them proves that what was pushed to a registry can be fetched
# and used by someone who has never seen this repository. That is a different
# question, and its failures land after a release rather than before it:
# a package left private, an architecture nobody published, an image that
# installs but cannot run.
#
# It runs the procedure from docs/guides/for-agents.md, so that page cannot
# drift into confident nonsense without this failing.
#
#   E2E_INSTALL_TAG      tag prefix to test (default: development)
#   E2E_INSTALL_VERSION  expected axiom_version(); skipped if unset
#   E2E_INSTALL_MAJORS   majors to check (default: "16 17 18")
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TAG="${E2E_INSTALL_TAG:-development}"
MAJORS="${E2E_INSTALL_MAJORS:-16 17 18}"
PG_IMAGE=ghcr.io/dhilipkumars/axiom-postgres
# The gateway is deliberately pinned to :development rather than the release
# tag. Both images are published by separate workflows on the same event, so
# requiring the release gateway here would make this gate race one. What this
# gate is for is the Postgres image; the gateway's own correctness is covered
# by every other gate in this suite, against a build from source.
GW_IMAGE=ghcr.io/dhilipkumars/axiom-gateway:development
CLUSTER=axiom-install
NODE="${CLUSTER}-control-plane"
CONTAINER=axiom-install-pg
# The newest major is what :latest becomes, so the full round trip runs there.
NEWEST="$(echo "$MAJORS" | tr ' ' '\n' | sort -n | tail -1)"

PLATFORM=()
[[ "$(uname -m)" =~ ^(arm64|aarch64)$ ]] && PLATFORM=(--platform linux/amd64)

log()  { printf '\n==> %s\n' "$*"; }
fail() { printf '\nE2E FAILED: %s\n' "$*" >&2; exit 1; }

# A private package and a public one are indistinguishable over the registry
# API -- both answer 401 -- so only a credential-free pull is a real test. Use
# an empty DOCKER_CONFIG rather than `docker logout`, which would otherwise
# sign the person running this out of ghcr.io on their own machine.
DOCKER_CONFIG="$(mktemp -d)"
export DOCKER_CONFIG
workdir="$(mktemp -d)"
cleanup() {
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  [[ "${E2E_INSTALL_KEEP:-0}" == "1" ]] || kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
  rm -rf "$DOCKER_CONFIG" "$workdir"
}
trap cleanup EXIT

log "pulling with no credentials (tag: $TAG)"
for pg in $MAJORS; do
  ref="${PG_IMAGE}:${TAG}-pg${pg}"
  out="$(docker pull "${PLATFORM[@]}" "$ref" 2>&1)" || {
    case "$out" in
      *denied*|*unauthorized*)
        fail "$ref is not publicly pullable. A new GHCR package is private until someone changes it: $out" ;;
      *"no matching manifest"*)
        fail "$ref has no image for this machine's architecture: $out" ;;
      *) fail "could not pull $ref: $out" ;;
    esac
  }
  echo "  $ref"
done

# Per-major, because the extension .so is the part that is built per major and
# is therefore the part that can be wrong for one of them.
for pg in $MAJORS; do
  log "pg${pg}: the image installs and reports itself"
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  docker run -d --name "$CONTAINER" "${PLATFORM[@]}" \
    -e POSTGRES_PASSWORD=install-test "${PG_IMAGE}:${TAG}-pg${pg}" >/dev/null
  for _ in $(seq 1 90); do
    docker exec "$CONTAINER" pg_isready -U postgres -h 127.0.0.1 >/dev/null 2>&1 && break
    sleep 2
  done
  docker exec "$CONTAINER" pg_isready -U postgres -h 127.0.0.1 >/dev/null 2>&1 \
    || fail "pg${pg}: never became ready"

  server="$(docker exec "$CONTAINER" psql -U postgres -tAc "SHOW server_version_num")"
  [[ "$server" == "${pg}"* ]] || fail "pg${pg}: image reports server_version_num $server"

  # Without this the extension installs and does nothing, so it is the single
  # most valuable assertion about a published image.
  preload="$(docker exec "$CONTAINER" psql -U postgres -tAc "SHOW shared_preload_libraries")"
  [[ "$preload" == "axiom" ]] || fail "pg${pg}: image does not preload axiom, got '$preload'"

  docker exec "$CONTAINER" psql -U postgres -tAc "CREATE EXTENSION axiom" >/dev/null \
    || fail "pg${pg}: CREATE EXTENSION failed"
  got="$(docker exec "$CONTAINER" psql -U postgres -tAc "SELECT axiom_version()")"
  [[ -n "$got" ]] || fail "pg${pg}: axiom_version() returned nothing"
  if [[ -n "${E2E_INSTALL_VERSION:-}" ]]; then
    [[ "$got" == "$E2E_INSTALL_VERSION" ]] \
      || fail "pg${pg}: axiom_version() = '$got', expected '$E2E_INSTALL_VERSION'"
  fi
  echo "  pg${pg}: server $server, axiom $got, preloaded"
done
docker rm -f "$CONTAINER" >/dev/null 2>&1 || true

# The guide tells readers to apply the manifests from raw.githubusercontent.com,
# so a 404 there breaks it for everyone even though nothing in this repository
# changed. Checked separately from the apply below, which uses the checkout, so
# a GitHub outage reports as what it is instead of failing the install.
log "the manifest URLs the guide publishes still resolve"
RAW=https://raw.githubusercontent.com/dhilipkumars/axiom/main/deploy/k8s
for f in gateway-rbac.yaml gateway-deployment.yaml; do
  code="$(curl -fsSL -o /dev/null -w '%{http_code}' "$RAW/$f" || true)"
  [[ "$code" == "200" ]] || fail "$RAW/$f returned $code; the guide's install step is broken"
done

log "pg${NEWEST}: the whole procedure from docs/guides/for-agents.md"
cd "$workdir"
kind get clusters | grep -qx "$CLUSTER" || kind create cluster --name "$CLUSTER" >/dev/null
kubectl --context "kind-$CLUSTER" wait --for=condition=Ready node --all --timeout=180s >/dev/null

mkdir -p certs
docker run --rm -v "$PWD/certs:/certs" -w /certs \
  --entrypoint /bin/sh alpine/openssl:3.3.3 -c "
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
      -days 365 -subj '/CN=axiom-dev-ca' -keyout ca.key -out ca.crt
    openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
      -subj '/CN=gateway' -keyout gateway.key -out gateway.csr
    printf 'subjectAltName=DNS:$NODE,DNS:axiom-gateway.axiom-system.svc,DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth\n' > san.cnf
    openssl x509 -req -in gateway.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
      -days 365 -extfile san.cnf -out gateway.crt
    rm -f gateway.csr san.cnf ca.srl ca.key
    chown $(id -u):$(id -g) ca.crt gateway.crt gateway.key
  " >/dev/null 2>&1 || fail "could not generate the keypair"

kubectl --context "kind-$CLUSTER" apply -f "$ROOT/deploy/k8s/gateway-rbac.yaml" >/dev/null
kubectl --context "kind-$CLUSTER" -n axiom-system delete secret axiom-gateway-tls --ignore-not-found >/dev/null
kubectl --context "kind-$CLUSTER" -n axiom-system create secret generic axiom-gateway-tls \
  --from-file=tls.crt=certs/gateway.crt --from-file=tls.key=certs/gateway.key >/dev/null
# The published gateway, not a local build: this gate is about what is on the
# registry, so side-loading a build from source would defeat it.
sed "s|image: ghcr.io/dhilipkumars/axiom-gateway:development|image: $GW_IMAGE|" \
  "$ROOT/deploy/k8s/gateway-deployment.yaml" | kubectl --context "kind-$CLUSTER" apply -f - >/dev/null
kubectl --context "kind-$CLUSTER" -n axiom-system rollout status deploy/axiom-gateway --timeout=180s >/dev/null \
  || fail "the published gateway image did not become ready"

docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
docker run -d --name "$CONTAINER" "${PLATFORM[@]}" --network kind \
  -e POSTGRES_PASSWORD=install-test -v "$PWD/certs:/certs:ro" \
  "${PG_IMAGE}:${TAG}-pg${NEWEST}" >/dev/null
for _ in $(seq 1 90); do
  docker exec "$CONTAINER" pg_isready -U postgres -h 127.0.0.1 >/dev/null 2>&1 && break
  sleep 2
done

docker exec -i "$CONTAINER" psql -U postgres -v ON_ERROR_STOP=1 >/dev/null <<SQL || fail "the guide's SQL failed"
CREATE EXTENSION axiom;
CREATE SERVER prod FOREIGN DATA WRAPPER axiom_fdw
  OPTIONS (endpoint 'https://${NODE}:30443', ca_cert '/certs/ca.crt');
CREATE USER MAPPING FOR CURRENT_USER SERVER prod;
CREATE SCHEMA k8s;
IMPORT FOREIGN SCHEMA k8s FROM SERVER prod INTO k8s;
SQL

log "the success criterion: SQL agrees with kubectl"
# The guide's own criterion. Compared against the cluster rather than asserted
# to be non-empty, because "returns some rows" would pass on a cache that is
# quietly serving the wrong collection.
sql_pods="$(docker exec "$CONTAINER" psql -U postgres -tAc \
  "SELECT name FROM k8s.pods WHERE namespace='kube-system' ORDER BY name")"
kubectl_pods="$(kubectl --context "kind-$CLUSTER" -n kube-system get pods \
  -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}' | sort)"
[[ -n "$sql_pods" ]] || fail "the pods query returned nothing"
if [[ "$sql_pods" != "$kubectl_pods" ]]; then
  fail "SQL and kubectl disagree.
SQL:
$sql_pods
kubectl:
$kubectl_pods"
fi
echo "$(wc -l <<<"$sql_pods" | tr -d ' ') pods, identical in SQL and kubectl"

log "INSTALL E2E PASSED"
