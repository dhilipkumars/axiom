# Did your patch regress? pgbench across Postgres 16, 17 and 18

Benchmark Postgres on Kubernetes, and see what it cost, from SQL. The lab is
two tables:

- **`lab.clusters`**: the Postgres clusters under test. CloudNativePG builds
  one from each row.
- **`lab.runs`**: `pgbench` runs against them. Each row becomes a Job.

While the runs go, a sampler records what the Postgres Pods use. One query then
puts each run's throughput beside the CPU and memory its Postgres used while
the benchmark was running. The example asks whether Postgres 17 or 18 regressed
against 16: three clusters with 2 CPUs each, differing only in their major
version, each measured by the same `pgbench` 18 client, one at a time.

```
      run       | cluster |         server_version          | cpu_limit | clients | state | tps  | latency_ms | pg_cpu_avg | tps_per_core | pg_cpu_peak | pg_memory_peak | samples | pgbench_cpu_avg | error
----------------+---------+---------------------------------+-----------+---------+-------+------+------------+------------+--------------+-------------+----------------+---------+-----------------+-------
 pg16-8-clients | pg16    | 16.15 (Debian 16.15-1.pgdg11+2) | 2         |       8 | done  | 2296 |      3.484 |       1.96 |         1173 |        1.96 | 173 MB         |       4 |            0.77 |
 pg17-8-clients | pg17    | 17.11 (Debian 17.11-1.pgdg11+2) | 2         |       8 | done  | 2316 |      3.454 |       1.96 |         1180 |        1.97 | 158 MB         |       6 |            0.77 |
 pg18-8-clients | pg18    | 18.4 (Debian 18.4-1.pgdg11+1)   | 2         |       8 | done  | 2247 |      3.561 |       1.96 |         1145 |        1.98 | 164 MB         |       5 |            0.74 |
(3 rows)
```

That is `results.sql` from the example-tests workflow's run on kind, with
`lab-example.sql`. How to read it:

- **Every server was CPU-bound.** Each Postgres used 1.96 of its 2 CPUs for
  the whole benchmark. So the comparison is throughput per unit of CPU, and
  `tps_per_core` shows it directly.
- **The three majors are within about 3% of each other,** at 1,145 to 1,180
  TPS per core. At this scale, with 90-second runs, each run once, that is no
  difference: nothing here says any of them regressed.
- **Running them one at a time is what made them comparable.** When the same
  three runs shared the node, at 500m each, they spread by about 6%. Most of
  that was the runs interfering with each other, not the versions.
- **The measurement is fair.** Every server was measured by the same
  `pgbench` 18 client. The client used about 0.75 cores, well short of what
  the node had to spare, so it was never the bottleneck.

**Check `pg_cpu_avg` before trusting a comparison.** If the servers were
meant to be CPU-bound and are well below their limit, something else set the
pace. On one CI run of this same lab, every server stayed under 0.65 of its 2
CPUs and throughput halved: a slow disk or a busy host on that runner. The
numbers from a run like that compare the machine, not Postgres; discard it
and run again.

Every step is SQL through Axiom:

- **Clusters.** `clusters.sql` writes a CloudNativePG `Cluster` custom resource
  for each row of `lab.clusters`.
- **Benchmarks.** `launch.sql` writes a `Job` for the next queued run.
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
psql -f lab-example.sql                           # Postgres 16, 17 and 18 at 2 CPUs each; a run queued for each
psql -v namespace=regression-lab -f clusters.sql
```

Wait until the clusters are healthy:

```sql
SELECT name, status->>'phase' FROM lab.pg_clusters WHERE namespace = 'regression-lab';
```

Then start the sampler in one session and, once it has recorded the clusters,
the queue in another. metrics-server reports a new Pod only after its first
scrape, so a benchmark started before then loses its first windows:

```sql
SELECT cluster, count(*) FROM lab.usage WHERE role = 'postgres' GROUP BY cluster;  -- a row per cluster
```

```
$ psql -v namespace=regression-lab             $ psql -v namespace=regression-lab
=> \i sample.sql                               => \i launch.sql
=> \watch 5                                    => \watch 10
```

```sh
psql -v namespace=regression-lab -f results.sql   # again, until every run is done
```

`\watch` repeats the last statement until you interrupt it; it needs an
interactive session, since `psql -f` runs a file once and exits.

**The runs are a queue.** `launch.sql` starts the oldest run in `lab.runs`
that has no Job yet, and only when no other run is unfinished. So under
`\watch` it is the scheduler: each run gets the node to itself, and the next
one starts as soon as the last one finishes. Queue more with an `INSERT` into
`lab.runs`; they run in the order they were queued (`queued_at`).

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

- **One run at a time, lab-wide.** Runs never overlap, not even on different
  clusters. That keeps them from competing for the same nodes, and keeps
  `pgbench -i` from rebuilding tables under a run in progress. The cost is
  time: three 90-second runs take about six minutes.
- **A run that never finishes holds up the queue.** A run whose Pod can't
  start, such as one naming a cluster that doesn't exist, stays unfinished,
  and `results.sql` shows it as `waiting` with the reason. The queue waits for
  its Job, not its row, so clear the Job:

  ```sql
  -- Retry it: the next launch.sql starts it again.
  DELETE FROM lab.jobs WHERE namespace = 'regression-lab' AND name = 'bench-<run>';
  -- Drop it: delete the Job as above, then the run.
  DELETE FROM lab.runs WHERE run = '<run>';
  ```

  Deleting only the row leaves the Job unfinished, and the queue stuck.
- **Termination messages are capped at 4 KiB.** That's enough for the JSON
  result, or the tail of an error. Reading whole logs from SQL is #105.
- **It acts as the gateway.** `rbac.yaml` lets the gateway create Clusters and
  Jobs in `regression-lab`. Every SQL role that can use the server acts as the
  gateway (#71), so any of them can do the same. Keep it a lab.
- **These numbers are not a benchmark.** The table above comes from a CI
  machine: a four-vCPU virtual machine shared with other tenants, short
  runs, each run once.
  Differences between majors at that scale are mostly noise. For a real
  comparison, pin each cluster to a dedicated node, run longer, and repeat each
  run several times.
