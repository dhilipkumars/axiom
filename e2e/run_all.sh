#!/usr/bin/env bash
# Runs every E2E gate in PLAN.md order against one build and one kind cluster.
#
# The per-gate scripts each stand up everything they need, which makes them
# usable standalone but means running all five pays for five image builds and
# five clusters. The build dominates: the Phase 0 gate needs no cluster at all
# and still takes ~11 minutes in CI, almost entirely compiling cargo-pgrx and
# the extension inside Docker. This driver pays that once.
#
# Each gate still gets a *fresh compose stack*. Sharing the running containers
# would save only ~20s per gate once the build is shared, and would break tests
# that assert on absolute counts of gateway log lines (see the subscribe_list
# assertion in watch_test.sh) as well as leak shared-memory cache state between
# gates. Isolation is worth more than those seconds.
#
#   ./e2e/run_all.sh                 # every gate, oldest first
#   ./e2e/run_all.sh watch crd       # just these, still sharing build + cluster
#
# Environment knobs:
#   E2E_KIND_KEEP=1   leave the cluster up afterwards (implied while gates run)
#   E2E_NO_BUILD=1    reuse already-built images instead of building once here
#   E2E_TIMEOUT_SECS  passed through to each gate
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
source "$here/lib/stack.sh"
source "$here/lib/kind.sh"

# PLAN.md order. A later gate is only meaningful if the earlier ones passed,
# so the run stops at the first failure (docs/RULES.md §4, regression gate).
ALL_GATES=(ping pods configmaps watch crd)
GATES=("${@:-}")
[[ -z "${GATES[*]}" ]] && GATES=("${ALL_GATES[@]}")

# The caller's intent for the cluster after the run; gates always keep it.
KEEP_CLUSTER_AT_END="${E2E_KIND_KEEP:-0}"

# hms SECONDS -> "1m03s", for the timing summary.
hms() { printf '%dm%02ds' $(( $1 / 60 )) $(( $1 % 60 )); }

suite_teardown() {
  local rc=$?
  if [[ "$KEEP_CLUSTER_AT_END" == "1" ]]; then
    log "E2E_KIND_KEEP=1, leaving kind cluster $E2E_KIND_CLUSTER"
  else
    E2E_KIND_KEEP=0 kind_down
  fi
  return $rc
}
trap suite_teardown EXIT

suite_start=$SECONDS

# --- build once -------------------------------------------------------------
# Built from the base compose file only: the kind overlay changes the gateway's
# command, volumes and networks, never its image, and it needs E2E_KUBE_DIR
# which does not exist until kind_up has run.
if [[ "${E2E_NO_BUILD:-0}" == "1" ]]; then
  log "E2E_NO_BUILD=1, reusing existing images"
  build_secs=0
else
  log "building images once for the whole suite"
  t0=$SECONDS
  compose build >/dev/null || fail "image build failed"
  build_secs=$(( SECONDS - t0 ))
  log "build took $(hms "$build_secs")"
fi

# --- one cluster for every gate ---------------------------------------------
t0=$SECONDS
kind_up
cluster_secs=$(( SECONDS - t0 ))
log "cluster ready in $(hms "$cluster_secs")"

# Gates must reuse what this driver just created rather than redoing it. Their
# own teardown still runs, so each gate gets a clean compose stack.
export E2E_NO_BUILD=1
export E2E_KIND_KEEP=1

# --- run the gates ----------------------------------------------------------
names=() times=()
for gate in "${GATES[@]}"; do
  script="$here/${gate}_test.sh"
  [[ -x "$script" ]] || fail "no such gate: $gate (expected $script)"
  log "gate: $gate"
  t0=$SECONDS
  "$script" || fail "gate '$gate' failed"
  names+=("$gate")
  times+=($(( SECONDS - t0 )))
done

# --- summary ----------------------------------------------------------------
total=$(( SECONDS - suite_start ))
printf '\n==> E2E SUITE PASSED\n\n'
printf '    %-14s %s\n' "build" "$(hms "$build_secs")"
printf '    %-14s %s\n' "cluster" "$(hms "$cluster_secs")"
for i in "${!names[@]}"; do
  printf '    %-14s %s\n' "${names[$i]}" "$(hms "${times[$i]}")"
done
printf '    %-14s %s\n' "TOTAL" "$(hms "$total")"
printf '\n    (%d gates sharing one build and one cluster)\n\n' "${#names[@]}"
