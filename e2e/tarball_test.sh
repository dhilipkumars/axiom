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

# Every tarball up front, not lazily inside the loop. The wrong-major check
# needs another major's tarball to exist, and building them on demand meant the
# first major never had one -- so that check silently did not run for it.
for pg in $MAJORS; do
  # Always repackaged, never reused. A tarball left in dist/ from before a
  # source edit would make this pass against code that is no longer there, and
  # the version only changes at release, so the filename cannot show it.
  # Docker's layer cache makes the rebuild cheap when nothing changed.
  log "packaging pg${pg} (${ARCH})"
  "$ROOT/scripts/package-extension" "$pg" "$ARCH" >/dev/null \
    || fail "pg${pg}: could not build the tarball"
done

for pg in $MAJORS; do
  tarball="$ROOT/dist/axiom-${VERSION}-pg${pg}-linux-${ARCH}.tar.gz"

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
  # INSTALL.md's own commands, through pg_config, rather than a paraphrase:
  # the instructions are what is under test. A path that pg_config disagrees
  # with is the failure this is looking for.
  docker exec "$CONTAINER" sh -c '
    set -e
    cd /tmp && tar -xzf axiom.tar.gz && cd axiom-*/
    cp usr/lib/postgresql/*/lib/axiom.so       "$(pg_config --pkglibdir)/"
    cp usr/share/postgresql/*/extension/axiom* "$(pg_config --sharedir)/extension/"
  ' || fail "pg${pg}: installing per INSTALL.md failed"

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

  server="$(docker exec "$CONTAINER" psql -U postgres -tAc 'SHOW server_version_num')"
  [[ "$server" == "${pg}"* ]] || fail "pg${pg}: server reports $server"
  # Reported here, while it is still true. The wrong-major block below stops
  # this container on purpose, so anything printed after it claiming a running
  # worker would be describing a corpse.
  echo "  pg${pg}: installed into stock postgres:${pg}, axiom ${got}, worker running"

  # A tarball built for a different major must fail loudly, not half-work.
  #
  # Tested by restarting with the wrong library preloaded, which is the real
  # contract: the postmaster loads it at startup and must refuse to come up.
  #
  # An earlier version used `LOAD 'axiom'` in a running server, and it passed
  # for the wrong reason. The server is preloaded by this point, so a fresh
  # backend running LOAD hits `_PG_init`'s own guard -- "axiom must be loaded
  # through shared_preload_libraries" -- and fails regardless of which major
  # the library was built for. It proved the preload error, not a mismatch.
  other=""
  for cand in $MAJORS; do [[ "$cand" != "$pg" ]] && { other="$cand"; break; }; done
  if [[ -z "$other" ]]; then
    echo "  pg${pg}: only one major selected, skipping the wrong-major check"
  else
    other_tar="$ROOT/dist/axiom-${VERSION}-pg${other}-linux-${ARCH}.tar.gz"
    [[ -f "$other_tar" ]] || fail "pg${pg}: no pg${other} tarball to test the mismatch with"
    docker cp "$other_tar" "$CONTAINER:/tmp/other.tar.gz" >/dev/null
    docker exec "$CONTAINER" sh -c '
      set -e
      cd /tmp && rm -rf wrong && mkdir wrong && tar -xzf other.tar.gz -C wrong
      cp wrong/axiom-*/usr/lib/postgresql/*/lib/axiom.so "$(pg_config --pkglibdir)/axiom.so"
    ' || fail "pg${pg}: could not stage the pg${other} library"
    docker restart "$CONTAINER" >/dev/null 2>&1 || true
    # Postgres refuses the library during startup and the container exits, so
    # watch for that rather than polling readiness for a fixed time. Waiting
    # out a timeout to prove a negative spent 30s per major for nothing.
    up=unknown
    for _ in $(seq 1 15); do
      state="$(docker inspect -f '{{.State.Status}}' "$CONTAINER" 2>/dev/null || echo gone)"
      [[ "$state" == "exited" || "$state" == "gone" ]] && { up=no; break; }
      docker exec "$CONTAINER" pg_isready -U postgres -h 127.0.0.1 >/dev/null 2>&1 \
        && { up=yes; break; }
      sleep 2
    done
    [[ "$up" == "no" ]] \
      || fail "pg${pg}: started with a pg${other} library preloaded, which should be impossible (state: ${up})"
    # For the right reason. Deliberately not matching the bare filename: a
    # permission or mount error also names axiom.so, and would then pass as an
    # ABI rejection. Only the messages Postgres uses when a library it loaded
    # is the wrong one count.
    logs="$(docker logs "$CONTAINER" 2>&1 | tail -40)"
    grep -qiE 'incompatible library|undefined symbol|could not load library' <<<"$logs" \
      || fail "pg${pg}: refused to start, but not for an incompatible library: $(tail -5 <<<"$logs")"
    echo "  pg${pg}: a pg${other} library preloaded stops the postmaster starting"
  fi

done

log "TARBALL E2E PASSED"
