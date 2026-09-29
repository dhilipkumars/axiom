-- Start one pgbench Job per row of lab.matrix that has no Job yet.
--
--   psql -v namespace=regression-lab -f launch.sql
--
-- The Job is written whole through `raw`, so every field it needs is spelled
-- exactly as Kubernetes expects. Each container starts its own throwaway
-- Postgres from the image under test, runs pgbench against it, and reports
-- the result as its termination message -- which Kubernetes keeps in the
-- Pod's status, where SQL can read it back without log access or a network
-- path to this database. On failure the message is the last of the
-- container's output instead (FallbackToLogsOnError), so the error is
-- readable the same way.
\set ON_ERROR_STOP on

INSERT INTO lab.jobs (namespace, name, raw)
SELECT :'namespace', 'bench-' || m.run, jsonb_build_object(
  'apiVersion', 'batch/v1',
  'kind', 'Job',
  'metadata', jsonb_build_object('labels', jsonb_build_object('axiom-lab/run', m.run)),
  'spec', jsonb_build_object(
    -- A failed benchmark is a result to read, not something to retry.
    'backoffLimit', 0,
    'template', jsonb_build_object(
      'metadata', jsonb_build_object('labels', jsonb_build_object('axiom-lab/run', m.run)),
      'spec', jsonb_build_object(
        'restartPolicy', 'Never',
        'containers', jsonb_build_array(jsonb_build_object(
          'name', 'pgbench',
          'image', m.image,
          'imagePullPolicy', 'IfNotPresent',
          'terminationMessagePolicy', 'FallbackToLogsOnError',
          'env', jsonb_build_array(
            jsonb_build_object('name', 'PG_SETTINGS', 'value', m.settings),
            jsonb_build_object('name', 'CLIENTS', 'value', m.clients::text),
            jsonb_build_object('name', 'SECONDS_TO_RUN', 'value', m.seconds::text)),
          'command', jsonb_build_array('bash', '-c', $script$
set -euo pipefail
data=/tmp/pgdata
mkdir -p "$data" && chown postgres "$data"
gosu postgres initdb -D "$data" -A trust >/dev/null
# pg_ctl -w returns once the server accepts connections.
gosu postgres pg_ctl -D "$data" -o "$PG_SETTINGS" -w -l /tmp/postgres.log start >/dev/null \
  || { tail -20 /tmp/postgres.log; exit 1; }
gosu postgres pgbench -q -i -s 1 postgres >/tmp/pgbench-init.log 2>&1 \
  || { cat /tmp/pgbench-init.log; exit 1; }
# Under set -e a failing assignment would exit before its output is printed,
# leaving the termination message empty; print it, then fail.
out="$(gosu postgres pgbench -c "$CLIENTS" -j "$CLIENTS" -T "$SECONDS_TO_RUN" postgres 2>&1)" \
  || { echo "$out"; exit 1; }
tps="$(sed -n 's/^tps = \([0-9.]*\).*/\1/p' <<<"$out" | head -1)"
lat="$(sed -n 's/^latency average = \([0-9.]*\) ms.*/\1/p' <<<"$out" | head -1)"
if [[ -z "$tps" || -z "$lat" ]]; then echo "$out"; exit 1; fi
printf '{"tps": %s, "latency_ms": %s, "server_version": "%s"}' \
  "$tps" "$lat" "$(gosu postgres postgres --version | awk '{print $3}')" > /dev/termination-log
$script$)))))))
FROM lab.matrix m
WHERE NOT EXISTS (
  SELECT 1 FROM lab.jobs j
   WHERE j.namespace = :'namespace' AND j.name = 'bench-' || m.run);
