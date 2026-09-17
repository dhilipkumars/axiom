#!/usr/bin/env bash
# Tarball E2E: the downloadable artifact installs into a *stock* Postgres.
#
# Every other gate runs the image, which already has the extension in place and
# already preloads it. That proves nothing about the tarball, whose whole
# audience is someone who cannot use the image because they already run
# Postgres. What can go wrong is exactly what the image hides: files unpacked
# to paths this Postgres does not consult, a .so built against a different
# major or a different libc, or install instructions that omit the preload.
#
#   E2E_TARBALL_MAJORS   majors to check (default: "16 17 18")
#   E2E_TARBALL_KEEP=1   leave the container up on failure
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MAJORS="${E2E_TARBALL_MAJORS:-16 17 18}"
CONTAINER=axiom-tarball-test
VERSION="$("$ROOT/scripts/version")"
case "$(uname -m)" in
  x86_64) ARCH=amd64 ;;
  arm64|aarch64) ARCH=arm64 ;;
  *) echo "unsupported architecture $(uname -m)" >&2; exit 2 ;;
esac

log()  { printf '\n==> %s\n' "$*"; }
fail() {
  printf '\nE2E FAILED: %s\n' "$*" >&2
  docker logs "$CONTAINER" 2>&1 | tail -30 >&2 || true
  [[ "${E2E_TARBALL_KEEP:-0}" == "1" ]] || docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  exit 1
}
cleanup() { [[ "${E2E_TARBALL_KEEP:-0}" == "1" ]] || docker rm -f "$CONTAINER" >/dev/null 2>&1 || true; }
trap cleanup EXIT

for pg in $MAJORS; do
  tarball="$ROOT/dist/axiom-${VERSION}-pg${pg}-linux-${ARCH}.tar.gz"
  [[ -f "$tarball" ]] || "$ROOT/scripts/package-extension" "$pg" "$ARCH" >/dev/null \
    || fail "pg${pg}: could not build the tarball"

  log "pg${pg}: unpacking the tarball into a stock postgres:${pg}"
  docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
  # Stock upstream image, not ours: the point is that the artifact stands on
  # its own, against a Postgres that has never heard of Axiom.
  docker run -d --name "$CONTAINER" -e POSTGRES_PASSWORD=tarball-test \
    "postgres:${pg}-bookworm" >/dev/null || fail "pg${pg}: could not start stock postgres"
  for _ in $(seq 1 90); do
    docker exec "$CONTAINER" pg_isready -U postgres -h 127.0.0.1 >/dev/null 2>&1 && break
    sleep 2
  done
  docker exec "$CONTAINER" pg_isready -U postgres -h 127.0.0.1 >/dev/null 2>&1 \
    || fail "pg${pg}: stock postgres never became ready"

  # Exactly what INSTALL.md tells a reader to do, so the instructions are what
  # is under test and not a paraphrase of them.
  docker cp "$tarball" "$CONTAINER:/tmp/axiom.tar.gz" >/dev/null
  docker exec "$CONTAINER" sh -c '
    set -e
    cd /tmp && tar -xzf axiom.tar.gz
    cp -r axiom-*/usr/. /usr/' || fail "pg${pg}: unpacking failed"

  log "pg${pg}: without the preload it refuses, naming the setting"
  out="$(docker exec "$CONTAINER" psql -U postgres -c 'CREATE EXTENSION axiom;' 2>&1 || true)"
  grep -q 'shared_preload_libraries' <<<"$out" \
    || fail "pg${pg}: expected the preload error, got: $out"

  log "pg${pg}: with the preload it installs and runs"
  # $PGDATA from inside the container, not a hardcoded path: postgres:18 moved
  # it to /var/lib/postgresql/18/docker while 16 and 17 use
  # /var/lib/postgresql/data, so assuming either one silently skips a major.
  docker exec "$CONTAINER" sh -c \
    "printf \"shared_preload_libraries = 'axiom'\\n\" >> \"\$PGDATA/postgresql.conf\"" \
    || fail "pg${pg}: could not append to \$PGDATA/postgresql.conf"
  docker restart "$CONTAINER" >/dev/null
  for _ in $(seq 1 90); do
    docker exec "$CONTAINER" pg_isready -U postgres -h 127.0.0.1 >/dev/null 2>&1 && break
    sleep 2
  done
  docker exec "$CONTAINER" pg_isready -U postgres -h 127.0.0.1 >/dev/null 2>&1 \
    || fail "pg${pg}: did not come back after the restart -- a .so that cannot load takes the postmaster with it"

  preload="$(docker exec "$CONTAINER" psql -U postgres -tAc 'SHOW shared_preload_libraries')"
  [[ "$preload" == "axiom" ]] || fail "pg${pg}: preload is '$preload'"
  docker exec "$CONTAINER" psql -U postgres -tAc 'CREATE EXTENSION axiom' >/dev/null \
    || fail "pg${pg}: CREATE EXTENSION failed after preloading"

  got="$(docker exec "$CONTAINER" psql -U postgres -tAc 'SELECT axiom_version()')"
  [[ "$got" == "$VERSION" ]] || fail "pg${pg}: axiom_version() = '$got', expected '$VERSION'"
  # The extension is more than one function: the worker must be running and its
  # shared memory mapped, which is what a mislinked .so would fail at.
  workers="$(docker exec "$CONTAINER" psql -U postgres -tAc \
    "SELECT count(*) FROM pg_stat_activity WHERE backend_type = 'axiom gateway pinger'")"
  [[ "$workers" == "1" ]] || fail "pg${pg}: the background worker is not running (got '$workers')"
  docker exec "$CONTAINER" psql -U postgres -tAc 'SELECT count(*) FROM axiom_watch_status()' >/dev/null \
    || fail "pg${pg}: axiom_watch_status() failed, so shared memory was not mapped"

  # A tarball for the wrong major must not silently half-work.
  server="$(docker exec "$CONTAINER" psql -U postgres -tAc 'SHOW server_version_num')"
  [[ "$server" == "${pg}"* ]] || fail "pg${pg}: server reports $server"
  echo "  pg${pg}: installed into stock postgres:${pg}, axiom ${got}, worker running"
done

log "TARBALL E2E PASSED"
