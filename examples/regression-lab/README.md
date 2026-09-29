# A Postgres regression lab

A benchmark matrix is a table: one row per run, with the Postgres build to
test, the settings to start it with, and how long to run. One `INSERT` turns
the rows into Kubernetes Jobs, and one query reads the results back.

```
     run      |         settings          | state  |   tps    | latency_ms | server_version | error
--------------+---------------------------+--------+----------+------------+----------------+-------------------------------------------
 buffers-16mb | -c shared_buffers=16MB    | done   | 2107.80  |      0.949 | 16.15          |
 buffers-64mb | -c shared_buffers=64MB    | done   | 2086.72  |      0.958 | 16.15          |
 broken       | -c shared_buffers=nonsense| failed |          |            |                | pg_ctl: could not start server ... FATAL: invalid value
              |                           |        |          |            |                | for parameter "shared_buffers": "nonsense"
```

That is `results.sql` from the e2e suite's run on kind (the resource columns
are left out; kind has no metrics-server). Five-second runs on a shared CI
machine, so read the shape, not the numbers.

Each Job starts a throwaway Postgres from the image under test, runs
`pgbench` against it, and reports the result as its **termination message**.
Kubernetes keeps that in the Pod's status, so SQL reads it straight back: no
log access, no results database the Job has to reach, nothing to collect. When
a run fails, the message is the tail of its output instead, and `results.sql`
shows it as the error.

## Running it

```sh
kubectl apply -f rbac.yaml       # lets the gateway create Jobs in regression-lab
psql -v server=<your axiom server> -f setup.sql
psql -v image=postgres:17-bookworm -f matrix-example.sql
psql -v namespace=regression-lab -f launch.sql
psql -v namespace=regression-lab -f results.sql   # again, until every run is done
```

`image` is any image with `postgres`, `initdb` and `pgbench` on its `PATH` and
`gosu` to drop root, which the official images have — including a build of
your own. To compare two builds, add a row per image. `launch.sql` starts a
Job only for runs that do not have one, so adding a row and launching again
runs just the new one.

### Resource use

Where metrics-server runs, `sample.sql` records each running benchmark's CPU
and memory into `lab.usage`, and `results.sql` shows each run's peak beside
its throughput. Run it on a loop while the matrix runs:

```sh
psql -v namespace=regression-lab -f sample.sql     # then, at the psql prompt: \watch 5
```

It has to sample while runs are going: `metrics.k8s.io` reports only pods
that are running, so a finished run has nothing left to join.
`axiom_quantity()` turns the API's `250m` and `64Mi` into numbers.

## Limits

- **Termination messages are capped at 4 KiB.** Enough for a JSON result or
  the tail of an error, not for `regression.diffs`. Reading whole logs from
  SQL is #105.
- **It acts as the gateway.** `rbac.yaml` lets the gateway create Jobs in
  `regression-lab`, and every SQL role that can use the server acts as the
  gateway (#71), so any of them can start containers there. Keep it a lab.
- **Numbers from a shared cluster are noisy.** Pin the Jobs to dedicated nodes
  and give them resource requests before reading much into a few percent.
- **The e2e suite checks the results, not the sampler.** It runs the matrix,
  the launch and the results as shipped on kind, which has no metrics-server;
  `sample.sql` is exercised only by hand.
