#!/usr/bin/env bash
# Ping E2E (Phase 0 in docs/PLAN.md): with gateway + Postgres running via
# compose, CREATE EXTENSION axiom and assert the background worker completes a
# TLS Ping round-trip to the gateway within a bounded time. No Kubernetes
# cluster is involved.
set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib/stack.sh"

stack_up

log "CREATE EXTENSION axiom"
psql_axiom "CREATE EXTENSION IF NOT EXISTS axiom;"
version="$(psql_axiom "SELECT axiom_version();")"
[[ -n "$version" ]] || fail "axiom_version() returned nothing"
echo "axiom_version() = $version"

log "asserting the worker reads the configured gateway endpoint"
endpoint="$(psql_axiom "SHOW axiom.gateway_endpoint;")"
[[ "$endpoint" == "$E2E_GATEWAY_ENDPOINT" ]] || fail "axiom.gateway_endpoint = '$endpoint', want '$E2E_GATEWAY_ENDPOINT'"

log "asserting background worker is registered"
workers="$(psql_axiom "SELECT count(*) FROM pg_stat_activity WHERE backend_type = 'axiom gateway pinger';")"
[[ "$workers" == "1" ]] || fail "expected 1 'axiom gateway pinger' worker in pg_stat_activity, got '$workers'"

log "waiting up to ${E2E_TIMEOUT_SECS}s for a successful Ping round-trip"
stack_wait_for_log "$E2E_SVC_POSTGRES" "axiom bgworker: ping ok endpoint=${E2E_GATEWAY_ENDPOINT}"

log "asserting the gateway serves TLS 1.3 with the generated CA"
tls_out="$(gateway_openssl)"
grep -q "Verify return code: 0 (ok)" <<<"$tls_out" || fail "gateway certificate did not verify against the generated CA"
grep -Eq "Protocol *: TLSv1.3" <<<"$tls_out"   || fail "gateway did not negotiate TLS 1.3"

log "PING E2E PASSED"
