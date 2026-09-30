-- Start a pgbench Job for each run of lab.runs that has none yet.
--
--   psql -v namespace=regression-lab -f launch.sql
--
-- Each Job connects to its cluster's read-write Service, <cluster>-rw, as the
-- `app` user, with the password CloudNativePG put in the <cluster>-app Secret.
-- Kubernetes injects that password into the container itself; neither SQL nor
-- the gateway ever reads the Secret. The client is the run's client_image, or
-- the cluster's own image when that is NULL.
--
-- The result is the container's termination message, which Kubernetes keeps in
-- the Pod's status for results.sql to read back: TPS, latency, and the UTC
-- times the timed run started and finished, which is the window results.sql
-- takes resource use from. On failure the message is the tail of the
-- container's output instead (FallbackToLogsOnError).
--
-- Runs on one cluster do not overlap: pgbench -i rebuilds its tables, which
-- would pull them out from under a run in progress. A run whose cluster
-- already has an unfinished Job is left for the next launch.
\set ON_ERROR_STOP on

INSERT INTO lab.jobs (namespace, name, raw)
SELECT DISTINCT ON (r.cluster)
  :'namespace', 'bench-' || r.run, jsonb_build_object(
  'apiVersion', 'batch/v1',
  'kind', 'Job',
  'metadata', jsonb_build_object('labels', jsonb_build_object(
    'axiom-lab/run', r.run, 'axiom-lab/cluster', r.cluster)),
  'spec', jsonb_build_object(
    -- A failed benchmark is a result to read, not something to retry.
    'backoffLimit', 0,
    'template', jsonb_build_object(
      'metadata', jsonb_build_object('labels', jsonb_build_object(
        'axiom-lab/run', r.run, 'axiom-lab/cluster', r.cluster)),
      'spec', jsonb_build_object(
        'restartPolicy', 'Never',
        'containers', jsonb_build_array(jsonb_build_object(
          'name', 'pgbench',
          'image', coalesce(r.client_image, c.image, 'ghcr.io/cloudnative-pg/postgresql:18.4'),
          'imagePullPolicy', 'IfNotPresent',
          'terminationMessagePolicy', 'FallbackToLogsOnError',
          'env', jsonb_build_array(
            jsonb_build_object('name', 'PGHOST', 'value', r.cluster || '-rw'),
            jsonb_build_object('name', 'PGUSER', 'value', 'app'),
            jsonb_build_object('name', 'PGDATABASE', 'value', 'app'),
            jsonb_build_object('name', 'PGPASSWORD', 'valueFrom', jsonb_build_object(
              'secretKeyRef', jsonb_build_object('name', r.cluster || '-app', 'key', 'password'))),
            jsonb_build_object('name', 'CLIENTS', 'value', r.clients::text),
            jsonb_build_object('name', 'SECONDS_TO_RUN', 'value', r.seconds::text),
            jsonb_build_object('name', 'SCALE', 'value', r.scale::text)),
          'command', jsonb_build_array('bash', '-c', $script$
set -euo pipefail
for _ in $(seq 150); do pg_isready -q && break; sleep 2; done
pg_isready -q || { echo "$PGHOST is not accepting connections after 300s"; exit 1; }
init="$(pgbench -i -s "$SCALE" 2>&1)" || { echo "$init"; exit 1; }
started="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
# Under set -e a failing assignment would exit before its output is printed,
# leaving the termination message empty; print it, then fail.
out="$(pgbench -c "$CLIENTS" -j "$CLIENTS" -T "$SECONDS_TO_RUN" 2>&1)" || { echo "$out"; exit 1; }
finished="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
tps="$(sed -n 's/^tps = \([0-9.]*\).*/\1/p' <<<"$out" | head -1)"
lat="$(sed -n 's/^latency average = \([0-9.]*\) ms.*/\1/p' <<<"$out" | head -1)"
if [[ -z "$tps" || -z "$lat" ]]; then echo "$out"; exit 1; fi
printf '{"tps": %s, "latency_ms": %s, "started_at": "%s", "finished_at": "%s", "server_version": "%s"}' \
  "$tps" "$lat" "$started" "$finished" "$(psql -XAtc 'SHOW server_version')" > /dev/termination-log
$script$)))))))
FROM lab.runs r
LEFT JOIN lab.clusters c ON c.name = r.cluster
WHERE NOT EXISTS (
        SELECT 1 FROM lab.jobs j
         WHERE j.namespace = :'namespace' AND j.name = 'bench-' || r.run)
  AND NOT EXISTS (
        SELECT 1 FROM lab.jobs j
         WHERE j.namespace = :'namespace'
           AND j.labels->>'axiom-lab/cluster' = r.cluster
           AND coalesce((j.status->>'succeeded')::int, 0) + coalesce((j.status->>'failed')::int, 0) = 0)
ORDER BY r.cluster, r.run;
