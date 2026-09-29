#!/usr/bin/env bash
# A Kubernetes operator in psql: keep reconcile.sql's desired state applied.
#
#   PGHOST=... PGUSER=... PGDATABASE=... ./operator.sh
#
# Connection settings come from the usual libpq environment variables.
#
#   SWEEP_SECONDS   reconcile at least this often, notified or not (default 30)
#   NOTIFY_DATABASE database the extension sends NOTIFY axiom_events in,
#                   the axiom.notify_database setting (default: PGDATABASE)
#
# It reconciles once at startup, then whenever a watched ConfigMap changes,
# and on a timer. The timer is what makes it correct: a notification is only a
# hint, and hints get lost -- nothing is queued while this loop is not
# listening, and the extension sends none for the objects a subscription finds
# when it starts. Every pass converges whatever it missed, so the notifications
# only make it faster.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
sweep="${SWEEP_SECONDS:-30}"
notify_db="${NOTIFY_DATABASE:-${PGDATABASE:-}}"

log() { printf '%s %s\n' "$(date -u +%H:%M:%S)" "$*"; }

reconcile() {
  local out
  if out="$(psql -X -q -At -v ON_ERROR_STOP=1 -f "$here/reconcile.sql" 2>&1)"; then
    log "reconciled ($1)${out:+: $out}"
  else
    # A conflict (40001) or an unreachable gateway: say so and carry on. The
    # next pass retries; an operator that exits on the first conflict would
    # stop converging exactly when the cluster is busy.
    log "reconcile failed ($1), retrying on the next pass: $out"
  fi
}

# Start the watch subscription. The extension subscribes on a watched table's
# first scan; until then there is nothing to notify about.
psql -X -q -At -v ON_ERROR_STOP=1 -c "SELECT count(*) FROM sqlop.configmaps_watched" >/dev/null

# One long-lived session that LISTENs, spoken to through two named pipes (not
# coproc, which the bash 3.2 that macOS ships does not have). psql prints a
# notification after the next command it runs, so the loop sends a one-second
# sleep and then a marker, and reads everything up to the marker.
pipes="$(mktemp -d)"
mkfifo "$pipes/in" "$pipes/out"
listener=""
trap '[[ -n "$listener" ]] && kill "$listener" 2>/dev/null; rm -rf "$pipes"' EXIT
# LC_ALL=C: psql translates its "Asynchronous notification" banner, and under
# another locale the loop would never recognise one and fall back to the sweep.
LC_ALL=C psql -X -At ${notify_db:+-d "$notify_db"} <"$pipes/in" >"$pipes/out" 2>&1 &
listener=$!
exec 3>"$pipes/in" 4<"$pipes/out"
echo "LISTEN axiom_events;" >&3

reconcile startup
last=$SECONDS
while :; do
  echo "SELECT pg_sleep(1);" >&3
  echo "SELECT 'axiom-operator-tick';" >&3
  changed=0
  while IFS= read -r line <&4; do
    [[ "$line" == axiom-operator-tick ]] && break
    # Every watched table notifies on the same channel; only ConfigMaps
    # matter here.
    if [[ "$line" == Asynchronous* && "$line" == *'"resource":"configmaps"'* ]]; then
      changed=1
    fi
  done
  if (( changed )); then
    reconcile notified
    last=$SECONDS
  elif (( SECONDS - last >= sweep )); then
    reconcile sweep
    last=$SECONDS
  fi
done
