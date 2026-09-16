#!/usr/bin/env bash
# Preload E2E: Axiom refuses to load unless it is in shared_preload_libraries,
# and says why in terms an operator can act on.
#
# This is the one behaviour `cargo pgrx test` structurally cannot cover: the
# harness adds the library to shared_preload_libraries for every test
# (`pg_test::postgresql_conf_options`), so no test in that suite has ever run
# in a backend where it was absent. Hence a container.
#
# Needs no Kubernetes cluster and no gateway -- it never gets as far as an RPC.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PG_MAJOR="${E2E_PG_MAJOR:-16}"
# run_all.sh sets E2E_PG_IMAGE to the image it already built, so the suite does
# not pay for the extension twice. Run alone, this builds its own.
IMAGE="${E2E_PG_IMAGE:-axiom-preload-test:pg${PG_MAJOR}}"
CONTAINER=axiom-preload-test

log()  { printf '\n==> %s\n' "$*"; }
fail() {
  printf '\nE2E FAILED: %s\n' "$*" >&2
  docker logs "$CONTAINER" 2>&1 | tail -30 >&2 || true
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  exit 1
}
cleanup() { docker rm -f "$CONTAINER" >/dev/null 2>&1 || true; }
trap cleanup EXIT

# Reuse a prebuilt image when one is named, so the suite does not pay for this
# build twice.
if [[ -z "${E2E_PG_IMAGE:-}" ]]; then
  log "building the extension image (pg${PG_MAJOR})"
  docker build -q -f "$ROOT/extension/Dockerfile" \
    --build-arg "PG_MAJOR=${PG_MAJOR}" -t "$IMAGE" "$ROOT" >/dev/null \
    || fail "could not build $IMAGE"
fi

# Start Postgres with the preload explicitly emptied. The image sets it, so
# this overrides the image's own default -- which is itself worth asserting,
# since the override is how compose and the other gates set their values.
start_pg() {
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  docker run -d --name "$CONTAINER" -e POSTGRES_PASSWORD=preload-test \
    "$IMAGE" "$@" >/dev/null || fail "could not start $IMAGE"
  for _ in $(seq 1 90); do
    docker exec "$CONTAINER" pg_isready -U postgres >/dev/null 2>&1 && return 0
    sleep 2
  done
  fail "postgres never became ready"
}

log "without the preload, CREATE EXTENSION fails with a message naming the setting"
start_pg postgres -c shared_preload_libraries=''
preload="$(docker exec "$CONTAINER" psql -U postgres -tAc "SHOW shared_preload_libraries")"
[[ -z "$preload" ]] || fail "expected an empty shared_preload_libraries, got '$preload'"

out="$(docker exec "$CONTAINER" psql -U postgres -c "CREATE EXTENSION axiom;" 2>&1 || true)"
# The point of the change: not a bare Postgres internal. Before this, the
# failure was "FATAL: cannot create PGC_POSTMASTER variables after startup",
# which names neither the extension nor the fix.
grep -q 'ERROR:  axiom must be loaded through shared_preload_libraries' <<<"$out" \
  || fail "expected the axiom preload ERROR, got: $out"
grep -q "shared_preload_libraries = 'axiom'" <<<"$out" \
  || fail "the detail must quote the setting to add; got: $out"
grep -qi 'PGC_POSTMASTER' <<<"$out" \
  && fail "the Postgres internal leaked into the user-facing message: $out"
echo "$out" | head -2

log "the session survives that error"
# The regression that matters most. As a FATAL this killed the connection, so
# an operator lost their session as well as their explanation. `psql` opens one
# connection for a file of statements, so a second statement running after the
# failed CREATE EXTENSION proves the backend is still alive.
alive="$(docker exec -i "$CONTAINER" psql -U postgres -tA <<'SQL'
CREATE EXTENSION axiom;
SELECT 'session survived';
SQL
)"
grep -q 'session survived' <<<"$alive" \
  || fail "the connection did not survive the error: $alive"

log "the extension is genuinely absent afterwards, not half-installed"
installed="$(docker exec "$CONTAINER" psql -U postgres -tAc \
  "SELECT count(*) FROM pg_extension WHERE extname = 'axiom'")"
[[ "$installed" == "0" ]] || fail "pg_extension has $installed axiom rows after a failed CREATE"

log "with the image's own preload, it installs and the worker runs"
start_pg
preload="$(docker exec "$CONTAINER" psql -U postgres -tAc "SHOW shared_preload_libraries")"
[[ "$preload" == "axiom" ]] || fail "the image should preload axiom, got '$preload'"
docker exec "$CONTAINER" psql -U postgres -tAc "CREATE EXTENSION axiom" >/dev/null \
  || fail "CREATE EXTENSION failed on a preloaded server"
version="$(docker exec "$CONTAINER" psql -U postgres -tAc "SELECT axiom_version()")"
[[ -n "$version" ]] || fail "axiom_version() returned nothing"
workers="$(docker exec "$CONTAINER" psql -U postgres -tAc \
  "SELECT count(*) FROM pg_stat_activity WHERE backend_type = 'axiom gateway pinger'")"
[[ "$workers" == "1" ]] || fail "expected the pinger worker to be running, got '$workers'"
echo "axiom_version() = $version, pinger running"

log "PRELOAD E2E PASSED"
