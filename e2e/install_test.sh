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
#   E2E_INSTALL_GATEWAY  gateway image to run (default: the development tag)
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TAG="${E2E_INSTALL_TAG:-development}"
MAJORS="${E2E_INSTALL_MAJORS:-16 17 18}"
PG_IMAGE=ghcr.io/dhilipkumars/axiom-postgres
# A release must test the gateway it is releasing, not last night's. The
# gateway publishes :development only on schedule and dispatch, so on a release
# that tag is whatever the last nightly pushed -- and if the release changed
# the wire protocol, testing against it would either fail for the wrong reason
# or pass while the real pair was broken.
#
# The two images are published by separate workflows on the same event, so the
# release gateway may not exist yet when this starts. That is a wait, not a
# reason to test the wrong thing: `wait_for_image` below bounds it.
GW_IMAGE="${E2E_INSTALL_GATEWAY:-ghcr.io/dhilipkumars/axiom-gateway:development}"
CLUSTER=axiom-install
NODE="${CLUSTER}-control-plane"
CONTAINER=axiom-install-pg

# Expanded as ${PLATFORM[@]+"${PLATFORM[@]}"} everywhere below, not
# "${PLATFORM[@]}": on x86_64 this array is empty, and bash 3.2 -- which is
# what macOS ships -- treats an empty array expansion as an unbound variable
# under `set -u`. bash 4.4 and later do not, so the plain form works in CI and
# fails only for a developer on an Intel Mac.
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
# Only delete a cluster this script created. Adopting one and then removing it
# would destroy a cluster someone else was using that happened to share the
# name, which is a bad trade for saving one `kind create`.
created_cluster=0
cleanup() {
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  if [[ "$created_cluster" == "1" && "${E2E_INSTALL_KEEP:-0}" != "1" ]]; then
    kind delete cluster --name "$CLUSTER" >/dev/null 2>&1 || true
  fi
  rm -rf "$DOCKER_CONFIG" "$workdir"
}
trap cleanup EXIT

# Bounded wait, because a sibling workflow may still be pushing it. Anything
# other than "not found" fails immediately: a private package or a missing
# architecture will not fix itself by waiting.
wait_for_image() {
  local ref="$1" waited=0 out
  shift
  local -a plat=("$@")
  while :; do
    out="$(docker pull ${plat[@]+"${plat[@]}"} "$ref" 2>&1)" && return 0
    case "$out" in
      *"not found"*|*"manifest unknown"*)
        (( waited >= 600 )) && fail "$ref never appeared after ${waited}s: $out"
        [[ "$waited" == 0 ]] && echo "  waiting for $ref to be published"
        sleep 15; waited=$(( waited + 15 )) ;;
      # The registry having a bad minute is not a broken release. Retried on
      # the same budget, and reported as itself if it never clears.
      *timeout*|*"connection reset"*|*"no such host"*|*"429"*|*"TLS handshake"*|*EOF*)
        (( waited >= 600 )) && fail "$ref: the registry kept failing for ${waited}s: $out"
        [[ "$waited" == 0 ]] && echo "  registry trouble pulling $ref, retrying"
        sleep 15; waited=$(( waited + 15 )) ;;
      # Deliberately fatal, and deliberately not retried: a package that is
      # private is the failure this gate exists to catch, and waiting would
      # turn a clear answer into a ten-minute timeout.
      *denied*|*unauthorized*)
        fail "$ref is not publicly pullable. A new GHCR package is private until someone changes it: $out" ;;
      *"no matching manifest"*)
        fail "$ref has no image for this machine's architecture: $out" ;;
      *) fail "could not pull $ref: $out" ;;
    esac
  done
}

log "pulling with no credentials (tag: $TAG)"
for pg in $MAJORS; do
  wait_for_image "${PG_IMAGE}:${TAG}-pg${pg}" ${PLATFORM[@]+"${PLATFORM[@]}"}
  echo "  ${PG_IMAGE}:${TAG}-pg${pg}"
done
# No platform override for the gateway: it is published multi-architecture, and
# kind pulls whichever slice the node needs. Forcing amd64 here would check a
# slice this machine's cluster will not run, and would hide a missing arm64
# publish on an arm64 host.
wait_for_image "$GW_IMAGE"
echo "  $GW_IMAGE"

# Per-major, because the extension .so is the part that is built per major and
# is therefore the part that can be wrong for one of them.
for pg in $MAJORS; do
  log "pg${pg}: the image installs and reports itself"
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  docker run -d --name "$CONTAINER" ${PLATFORM[@]+"${PLATFORM[@]}"} \
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
# so a 404 there breaks the published instructions even though nothing in this
# repository changed. Worth knowing about, but not worth failing a release for:
# a 5xx or a rate limit is GitHub having a bad day, and this gate is about
# whether our images install. A 404 is ours and is reported as a failure; any
# other non-200 is reported as unknown and does not stop the release.
#
# Always against main, deliberately: main is what the published guide points
# at, so on a branch this answers "is the instruction people are following
# broken", not "does my branch work".
log "the manifest URLs the published guide points at"
RAW=https://raw.githubusercontent.com/dhilipkumars/axiom/main/deploy/k8s
for f in gateway-rbac.yaml gateway-deployment.yaml; do
  code="$(curl -fsSL -o /dev/null -w '%{http_code}' --max-time 20 "$RAW/$f" || true)"
  case "$code" in
    200) echo "  $f 200" ;;
    404) fail "$RAW/$f is gone; the guide's install step is broken for everyone" ;;
    *)   echo "  $f returned '$code' -- could not check (not treated as a failure)" ;;
  esac
done

log "one cluster and one published gateway, shared by every major"
cd "$workdir"
if kind get clusters | grep -qx "$CLUSTER"; then
  echo "  reusing an existing cluster named $CLUSTER; it will not be deleted"
else
  kind create cluster --name "$CLUSTER" >/dev/null
  created_cluster=1
fi
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
sed "s|image: ghcr.io/dhilipkumars/axiom-gateway:.*|image: $GW_IMAGE|" \
  "$ROOT/deploy/k8s/gateway-deployment.yaml" | kubectl --context "kind-$CLUSTER" apply -f - >/dev/null
# The guide restarts here for a reason this script needs even more: on a reused
# cluster the Deployment is unchanged, so `apply` alone leaves the running pod
# holding the certificate from the previous run while the Secret has been
# replaced -- and the TLS handshake then fails several steps later, where the
# cause is no longer visible.
kubectl --context "kind-$CLUSTER" -n axiom-system rollout restart deploy/axiom-gateway >/dev/null
kubectl --context "kind-$CLUSTER" -n axiom-system rollout status deploy/axiom-gateway --timeout=180s >/dev/null \
  || fail "the published gateway image did not become ready"

# kube-system is the comparison set, so it has to stop moving before SQL and
# kubectl are asked the same question. Without this a CoreDNS pod appearing
# between the two queries fails a perfectly good image.
kubectl --context "kind-$CLUSTER" -n kube-system wait --for=condition=Ready pod --all --timeout=180s >/dev/null \
  || fail "kube-system never settled; the comparison below would be a coin toss"

# Every major, not just the newest. `promote` moves latest-pg16, latest-pg17
# and latest-pg18, and the extension is compiled separately for each against
# that major's headers -- so a scan that breaks on one and not the others is
# exactly the failure this gate should catch.
for pg in $MAJORS; do
  log "pg${pg}: the whole procedure from docs/guides/for-agents.md"
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  docker run -d --name "$CONTAINER" ${PLATFORM[@]+"${PLATFORM[@]}"} --network kind \
    -e POSTGRES_PASSWORD=install-test -v "$PWD/certs:/certs:ro" \
    "${PG_IMAGE}:${TAG}-pg${pg}" >/dev/null
  for _ in $(seq 1 90); do
    docker exec "$CONTAINER" pg_isready -U postgres -h 127.0.0.1 >/dev/null 2>&1 && break
    sleep 2
  done
  docker exec "$CONTAINER" pg_isready -U postgres -h 127.0.0.1 >/dev/null 2>&1 \
    || fail "pg${pg}: never became ready for the round trip"

  # The guide's statements, guards included: it publishes them as re-runnable,
  # so running a different, stricter version here would not be testing the
  # thing the page tells people to do.
  docker exec -i "$CONTAINER" psql -U postgres -v ON_ERROR_STOP=1 >/dev/null <<SQL || fail "pg${pg}: the guide's SQL failed"
CREATE EXTENSION IF NOT EXISTS axiom;
DROP SERVER IF EXISTS prod CASCADE;
CREATE SERVER prod FOREIGN DATA WRAPPER axiom_fdw
  OPTIONS (endpoint 'https://${NODE}:30443', ca_cert '/certs/ca.crt');
CREATE USER MAPPING FOR CURRENT_USER SERVER prod;
CREATE SCHEMA IF NOT EXISTS k8s;
IMPORT FOREIGN SCHEMA k8s FROM SERVER prod INTO k8s;
SQL

  # The guide's own post-import check, before the scan.
  tables="$(docker exec "$CONTAINER" psql -U postgres -tAc \
    "SELECT count(*) FROM information_schema.tables WHERE table_schema='k8s'")"
  [[ "${tables:-0}" -gt 0 ]] || fail "pg${pg}: the import created no tables"

  # The guide's success criterion, all four columns of it. `name` alone would
  # pass on an extension that had broken the promoted columns entirely:
  # `phase` and `node` come from status.phase and spec.nodeName, which is the
  # schema mapping this image exists to perform, and comparing only the key
  # skips it.
  #
  # Both sides are sorted the same way. Postgres ORDER BY follows the
  # database's collation and the shell's `sort` follows the caller's locale,
  # and hyphenated pod names are exactly where those two disagree, so a healthy
  # build would fail on some hosts and not others.
  #
  # kube-system is re-checked immediately before the query: the loop spans
  # several minutes across majors, and a pod replaced in between would
  # otherwise be read by one side and not the other.
  kubectl --context "kind-$CLUSTER" -n kube-system wait --for=condition=Ready pod --all --timeout=180s >/dev/null \
    || fail "pg${pg}: kube-system stopped being settled; the comparison would be a coin toss"
  sql_pods="$(docker exec "$CONTAINER" psql -U postgres -tAF'|' -c \
    "SELECT name, phase, node FROM k8s.pods WHERE namespace='kube-system' ORDER BY name COLLATE \"C\"")"
  kubectl_pods="$(kubectl --context "kind-$CLUSTER" -n kube-system get pods \
    -o jsonpath='{range .items[*]}{.metadata.name}|{.status.phase}|{.spec.nodeName}{"\n"}{end}' \
    | LC_ALL=C sort)"
  [[ -n "$sql_pods" ]] || fail "pg${pg}: the pods query returned nothing"
  if [[ "$sql_pods" != "$kubectl_pods" ]]; then
    fail "pg${pg}: SQL and kubectl disagree on name|phase|node.
SQL:
$sql_pods
kubectl:
$kubectl_pods"
  fi
  echo "  pg${pg}: $(wc -l <<<"$sql_pods" | tr -d ' ') pods, name/phase/node identical in SQL and kubectl"
done

log "INSTALL E2E PASSED"
