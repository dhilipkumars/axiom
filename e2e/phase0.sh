#!/usr/bin/env bash
# Phase 0 E2E (docs/PLAN.md): bring up gateway + Postgres via compose, CREATE
# EXTENSION axiom, and assert the background worker logs a successful TLS Ping
# round-trip within a bounded time. No Kubernetes cluster is involved.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
COMPOSE=(docker compose -f "$ROOT/deploy/compose/docker-compose.yml")
TIMEOUT_SECS="${E2E_TIMEOUT_SECS:-90}"
KEEP="${E2E_KEEP:-0}"

log() { printf '\n==> %s\n' "$*"; }
fail() { printf '\nE2E FAILED: %s\n' "$*" >&2; dump; exit 1; }
dump() {
  log "gateway logs"; "${COMPOSE[@]}" logs --no-color gateway || true
  log "postgres logs (axiom lines)"; "${COMPOSE[@]}" logs --no-color postgres | grep -i axiom || true
}
cleanup() {
  if [[ "$KEEP" == "1" ]]; then log "E2E_KEEP=1, leaving stack running"; return; fi
  log "tearing down"; "${COMPOSE[@]}" down -v --remove-orphans >/dev/null 2>&1 || true
}
trap cleanup EXIT

psql_in() { "${COMPOSE[@]}" exec -T postgres psql -v ON_ERROR_STOP=1 -U axiom -d axiom -At -c "$1"; }

log "building and starting stack"
"${COMPOSE[@]}" up -d --build --wait --wait-timeout "$TIMEOUT_SECS" || fail "stack did not become healthy"

log "CREATE EXTENSION axiom"
psql_in "CREATE EXTENSION IF NOT EXISTS axiom;"
version="$(psql_in "SELECT axiom_version();")"
[[ -n "$version" ]] || fail "axiom_version() returned nothing"
echo "axiom_version() = $version"

log "asserting background worker is registered"
workers="$(psql_in "SELECT count(*) FROM pg_stat_activity WHERE backend_type = 'axiom gateway pinger';")"
[[ "$workers" == "1" ]] || fail "expected 1 'axiom gateway pinger' worker in pg_stat_activity, got '$workers'"

log "waiting up to ${TIMEOUT_SECS}s for a successful Ping round-trip"
deadline=$((SECONDS + TIMEOUT_SECS))
# Capture first, then grep: with `pipefail`, `grep -q` exiting early would
# SIGPIPE `docker compose logs` and make a successful match look like a failure.
pg_logs() { "${COMPOSE[@]}" logs --no-color postgres 2>/dev/null || true; }
while :; do
  logs="$(pg_logs)"
  if grep -q "axiom bgworker: ping ok endpoint=https://gateway:8443" <<<"$logs"; then break; fi
  (( SECONDS < deadline )) || fail "no 'axiom bgworker: ping ok' log line within ${TIMEOUT_SECS}s"
  sleep 1
done
grep "axiom bgworker: ping ok" <<<"$logs" | tail -1

log "asserting the gateway serves TLS 1.3 with the generated CA"
"${COMPOSE[@]}" run --rm --no-deps --entrypoint /bin/sh certs -c \
  'echo | openssl s_client -connect gateway:8443 -CAfile /certs/ca.crt -servername gateway 2>/dev/null | grep -q "Verify return code: 0 (ok)"' \
  || fail "gateway certificate did not verify against the generated CA"
"${COMPOSE[@]}" run --rm --no-deps --entrypoint /bin/sh certs -c \
  'echo | openssl s_client -connect gateway:8443 -CAfile /certs/ca.crt -servername gateway 2>/dev/null | grep -q "Protocol *: TLSv1.3"' \
  || fail "gateway did not negotiate TLS 1.3"

log "PHASE 0 E2E PASSED"
