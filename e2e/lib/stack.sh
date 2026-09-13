#!/usr/bin/env bash
# Reusable E2E setup library: brings the local compose stack (gateway +
# Postgres with the axiom extension) up and down and exposes helpers for
# running SQL, reading logs, and waiting on conditions.
#
# Source this from a test script; do not execute it directly.
#
#   source "$(dirname "${BASH_SOURCE[0]}")/lib/stack.sh"
#   stack_up
#   psql_axiom "SELECT 1;"
#   stack_wait_for_log postgres "some line" 60
#
# Environment knobs (all optional):
#   E2E_TIMEOUT_SECS  overall wait budget for stack start / log waits (default 90)
#   E2E_KEEP=1        leave the stack running after the test (skips teardown)
#   E2E_NO_BUILD=1    skip `--build` on `up` (reuse existing images)
#   E2E_COMPOSE_OVERLAYS  extra compose files layered over the base stack

# Guard against double-sourcing.
[[ -n "${_AXIOM_E2E_STACK_LIB:-}" ]] && return 0
_AXIOM_E2E_STACK_LIB=1

E2E_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
E2E_COMPOSE_FILE="${E2E_COMPOSE_FILE:-$E2E_ROOT/deploy/compose/docker-compose.yml}"
# Space-separated extra compose files layered over the base (e.g. the kind overlay).
E2E_COMPOSE_OVERLAYS="${E2E_COMPOSE_OVERLAYS:-}"
E2E_TIMEOUT_SECS="${E2E_TIMEOUT_SECS:-90}"
E2E_KEEP="${E2E_KEEP:-0}"
E2E_NO_BUILD="${E2E_NO_BUILD:-0}"

# Names as they appear in docker-compose.yml.
E2E_SVC_POSTGRES="postgres"
E2E_SVC_GATEWAY="gateway"
E2E_SVC_CERTS="certs"
E2E_PG_USER="axiom"
E2E_PG_DB="axiom"
# Where Postgres dials the gateway. `compose` is the container on the compose
# network; `incluster` is the Deployment, reached through its NodePort on the
# kind node. Gates that need a cluster use `incluster`; the Phase 0 ping gate has
# no cluster and stays on `compose`.
# Set by the caller *before* sourcing this file: the endpoint is resolved here,
# once, and a gate that sets the mode afterwards would silently keep dialling
# the compose service. stack_up re-checks and fails loudly if that happens.
E2E_GATEWAY_MODE="${E2E_GATEWAY_MODE:-compose}"
E2E_GATEWAY_MODE_AT_SOURCE="$E2E_GATEWAY_MODE"
if [[ "$E2E_GATEWAY_MODE" == "incluster" ]]; then
  E2E_GATEWAY_ENDPOINT="https://${E2E_KIND_CLUSTER:-axiom-e2e}-control-plane:${E2E_GATEWAY_NODEPORT:-30443}"
else
  E2E_GATEWAY_ENDPOINT="https://gateway:8443"
fi

# --- output helpers ---------------------------------------------------------

log()  { printf '\n==> %s\n' "$*"; }
# fail: print a failure banner, dump diagnostics, exit 1 (teardown runs via trap).
fail() { printf '\nE2E FAILED: %s\n' "$*" >&2; stack_dump; exit 1; }

# --- compose wrapper --------------------------------------------------------

compose() {
  local files=(-f "$E2E_COMPOSE_FILE") f
  for f in $E2E_COMPOSE_OVERLAYS; do files+=(-f "$f"); done
  docker compose "${files[@]}" "$@"
}

# stack_dump: print gateway logs and the extension's Postgres log lines.
stack_dump() {
  # Through stack_logs, not compose directly: in incluster mode the compose
  # gateway is scaled to zero, so `compose logs` prints nothing and every
  # failure dump came back empty. That is the one moment the logs are wanted,
  # and it cost real time diagnosing a CI failure that had printed a blank
  # "gateway logs" heading.
  log "gateway logs";                 stack_logs "$E2E_SVC_GATEWAY" 2>/dev/null || true
  log "postgres logs (axiom lines)";  compose logs --no-color "$E2E_SVC_POSTGRES" 2>/dev/null | grep -i axiom || true
}

# stack_down: tear the stack down including volumes (the generated certs).
# Honours E2E_KEEP=1.
stack_down() {
  if [[ "$E2E_KEEP" == "1" ]]; then log "E2E_KEEP=1, leaving stack running"; return 0; fi
  log "tearing down"
  compose down -v --remove-orphans >/dev/null 2>&1 || true
}

# Extra teardown commands registered by tests/libraries, run after stack_down.
E2E_TEARDOWN_HOOKS=()
e2e_on_teardown() { E2E_TEARDOWN_HOOKS+=("$1"); }
e2e_teardown() {
  stack_down
  local h
  for h in ${E2E_TEARDOWN_HOOKS[@]+"${E2E_TEARDOWN_HOOKS[@]}"}; do "$h"; done
}

# stack_up: build (unless E2E_NO_BUILD=1) and start the stack, waiting until
# Postgres reports healthy. Registers stack_down to run on exit.
stack_up() {
  [[ "$E2E_GATEWAY_MODE" == "$E2E_GATEWAY_MODE_AT_SOURCE" ]] || fail \
    "E2E_GATEWAY_MODE was changed to '$E2E_GATEWAY_MODE' after lib/stack.sh was sourced; \
the endpoint is still '$E2E_GATEWAY_ENDPOINT'. Set it before the source line."
  # Preserve the script's exit status across teardown (bash 3.2 compatible).
  trap 'rc=$?; e2e_teardown; exit $rc' EXIT
  local build="--build"
  [[ "$E2E_NO_BUILD" == "1" ]] && build=""
  log "building and starting stack"
  # shellcheck disable=SC2086  # $build is intentionally empty or a single flag
  compose up -d $build --wait --wait-timeout "$E2E_TIMEOUT_SECS" \
    || fail "stack did not become healthy within ${E2E_TIMEOUT_SECS}s"
}

# --- Postgres helpers -------------------------------------------------------

# psql_axiom SQL: run one statement as the app user, unaligned tuples-only
# output, failing on the first SQL error.
psql_axiom() {
  compose exec -T "$E2E_SVC_POSTGRES" psql -q -v ON_ERROR_STOP=1 -U "$E2E_PG_USER" -d "$E2E_PG_DB" -At -c "$1"
}

# stack_logs SERVICE: full log of one service, never failing the caller.
# stack_logs SERVICE: one service's log. The gateway's comes from the Pod when
# it runs in-cluster, so callers do not have to know which mode they are in.
# Counting assertions should use axiom_gateway_stats() instead: a Pod restart
# starts a fresh log, and a restart mid-watch is exactly what gets tested.
stack_logs() {
  if [[ "$1" == "$E2E_SVC_GATEWAY" && "$E2E_GATEWAY_MODE" == "incluster" ]]; then
    kind_gateway_logs
    return 0
  fi
  compose logs --no-color "$1" 2>/dev/null || true
}

# stack_wait_for_log SERVICE PATTERN [TIMEOUT_SECS]: poll SERVICE's log until a
# line matches PATTERN (grep -E). Fails the test on timeout. Captures logs into a
# variable before grepping: with `pipefail`, `grep -q` exiting early would
# SIGPIPE `docker compose logs` and make a successful match look like a failure.
stack_wait_for_log() {
  local svc="$1" pattern="$2" timeout="${3:-$E2E_TIMEOUT_SECS}"
  local deadline=$((SECONDS + timeout)) logs
  while :; do
    logs="$(stack_logs "$svc")"
    if grep -Eq -- "$pattern" <<<"$logs"; then
      grep -E -- "$pattern" <<<"$logs" | tail -1
      return 0
    fi
    (( SECONDS < deadline )) || fail "no log line matching '$pattern' from $svc within ${timeout}s"
    sleep 1
  done
}

# --- gateway helpers --------------------------------------------------------

# gateway_openssl ARGS...: run `openssl s_client` against the gateway from a
# throwaway container on the compose network with the generated CA mounted.
# Prints s_client's output.
gateway_openssl() {
  compose run --rm --no-deps --entrypoint /bin/sh "$E2E_SVC_CERTS" -c \
    "echo | openssl s_client -connect gateway:8443 -CAfile /certs/ca.crt -servername gateway $* 2>/dev/null"
}
