# A Postgres regression lab

Benchmark Postgres on Kubernetes, and see what it cost, from SQL. The lab is
two tables:

- **`lab.clusters`**: the Postgres clusters under test. CloudNativePG builds
  one from each row.
- **`lab.runs`**: `pgbench` runs against them. Each row becomes a Job.

While the runs go, a sampler records what the Postgres Pods use. One query then
puts each run's throughput beside the CPU and memory its Postgres used while
the benchmark was running. The example asks whether Postgres 17 or 18 regressed
against 16: three clusters with the same 500m CPU limit, differing only in
their major version, each measured by the same `pgbench` 18 client.

```
       run       |  cluster   |         server_version          | cpu_limit | clients |  state  | tps | latency_ms | pg_cpu_avg | tps_per_core | pg_cpu_peak | pg_memory_peak | samples | pgbench_cpu_avg |                             error
-----------------+------------+---------------------------------+-----------+---------+---------+-----+------------+------------+--------------+-------------+----------------+---------+-----------------+---------------------------------------------------------------
 missing-cluster | pg-missing |                                 |           |       8 | waiting |     |            |            |              |             |                |         |                 | CreateContainerConfigError: secret "pg-missing-app" not found
 pg16-8-clients  | pg16       | 16.15 (Debian 16.15-1.pgdg11+2) | 500m      |       8 | done    | 587 |     13.630 |       0.50 |         1174 |        0.50 | 142 MB         |       3 |            0.17 |
 pg17-8-clients  | pg17       | 17.11 (Debian 17.11-1.pgdg11+2) | 500m      |       8 | done    | 596 |     13.423 |       0.50 |         1193 |        0.50 | 143 MB         |       3 |            0.17 |
 pg18-8-clients  | pg18       | 18.4 (Debian 18.4-1.pgdg11+1)   | 500m      |       8 | done    | 563 |     14.217 |       0.50 |         1127 |        0.50 | 148 MB         |       3 |            0.17 |
(4 rows)
```

That is `results.sql` from the example-tests workflow's run on kind, with
`lab-example.sql`. How to read it:

- **Every server was CPU-bound.** Each Postgres sat at exactly 0.50 cores for
  the whole benchmark, which is its limit. So the comparison is throughput
  per unit of CPU, and `tps_per_core` shows it directly.
- **The three majors land within about 6% of each other.** 17 is the fastest
  and 18 the slowest, at 1,193 and 1,127 TPS per core. On a shared CI runner
  with one-minute runs, run once, that is noise, not a finding. It is the
  kind of gap a real lab repeats runs to confirm or rule out.
- **The measurement is fair.** Every server was measured by the same
  `pgbench` 18 client, which used 0.17 cores, so the client was never the
  bottleneck. Memory grows slightly with each major, from 142 to 148 MB.
- **A mistake shows up as a reason, not a hang.** The run against a cluster
  that was never created is `waiting`, and the error says exactly why.

Every step is SQL through Axiom:

- **Clusters.** `clusters.sql` writes a CloudNativePG `Cluster` custom resource
  for each row of `lab.clusters`.
- **Benchmarks.** `launch.sql` writes a `Job` for each run.
- **Usage.** `sample.sql` reads `metrics.k8s.io`.
- **Results.** `results.sql` reads each run's result back from its Pod's
  termination message, and joins it to the samples.

Nothing is collected, exported or scraped outside Postgres.

## Running it

It needs [CloudNativePG](https://cloudnative-pg.io) and, for the resource
columns, [metrics-server](https://github.com/kubernetes-sigs/metrics-server).
The gateway must serve the kinds it uses:
`--serve ...,jobs.batch,clusters.postgresql.cnpg.io,pods.metrics.k8s.io`.

```sh
kubectl apply -f rbac.yaml                        # lets the gateway create Clusters and Jobs in regression-lab
psql -v server=<your axiom server> -f setup.sql
psql -f lab-example.sql                           # Postgres 16, 17 and 18 at 500m each; a run against each
psql -v namespace=regression-lab -f clusters.sql
```

Wait until the clusters are healthy:

```sql
SELECT name, status->>'phase' FROM lab.pg_clusters WHERE namespace = 'regression-lab';
```

Then start the sampler in one session and the runs from another:

```
$ psql -v namespace=regression-lab
=> \i sample.sql
=> \watch 5
```

```sh
psql -v namespace=regression-lab -f launch.sql
psql -v namespace=regression-lab -f results.sql   # again, until the runs are done
```

`\watch` repeats the sample until you interrupt it. It needs an interactive
session: `psql -f` would run the file once and exit.

## How the numbers are matched

- **The benchmark window.** Each Job records the UTC times immediately before
  and after the timed `pgbench -T` run, and reports them with its result.
  `results.sql` counts only the samples whose whole window falls between them.
  Waiting for the cluster and building the pgbench tables don't dilute the
  average. `samples` shows how many samples that left.
- **Only real samples.** metrics-server reports a Pod about every 15 seconds,
  as an average over that window. `sample.sql` keys each sample on its own
  timestamp, so polling faster records nothing twice.
- **The right Pods.** Postgres is CloudNativePG's instance Pods
  (`cnpg.io/podRole=instance`). Its one-off `initdb` Job carries the cluster's
  label too, and is left out. The `pgbench` client's own CPU is shown
  separately, so you can see whether the client, not the server, was the
  bottleneck.
- **No password in SQL.** Each Job reads its cluster's password from the
  `<cluster>-app` Secret that CloudNativePG creates, through `secretKeyRef`.
  Kubernetes injects it into the container; neither SQL nor the gateway ever
  reads a Secret.

## Other experiments

The tables are the experiment. Change the rows, then run `clusters.sql` and
`launch.sql` again.

- **What more CPU buys.** Give two clusters the same image and different
  `cpu` limits. On kind, a 500m and a 2-CPU Postgres 17 ran 590 and 2,418 TPS.
  The 500m one sat at exactly 0.50 cores for the whole run.
- **A build of your own.** Put your image in `lab.clusters.image`. Any image
  CloudNativePG can run works; for `pgbench`, set `client_image` to one that
  has it.
- **A configuration change.** Vary `parameters`, such as `shared_buffers` or
  `work_mem`, across clusters that are otherwise the same.

## Limits

- **Runs on one cluster take turns.** `pgbench -i` rebuilds its tables, so
  `launch.sql` won't start a run while another run on the same cluster is
  unfinished. Launch again once it's done.
- **Termination messages are capped at 4 KiB.** That's enough for the JSON
  result, or the tail of an error. Reading whole logs from SQL is #105.
- **It acts as the gateway.** `rbac.yaml` lets the gateway create Clusters and
  Jobs in `regression-lab`. Every SQL role that can use the server acts as the
  gateway (#71), so any of them can do the same. Keep it a lab.
- **These numbers are not a benchmark.** The table above comes from a CI
  machine: three clusters sharing four vCPUs, one-minute runs, run once.
  Differences between majors at that scale are mostly noise. For a real
  comparison, pin each cluster to a dedicated node, run longer, and repeat each
  run several times.
