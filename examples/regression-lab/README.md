# A Postgres regression lab

Benchmark Postgres on Kubernetes, and see what it cost, from SQL. The lab is
two tables:

- **`lab.clusters`**: the Postgres clusters under test. CloudNativePG builds
  one from each row.
- **`lab.runs`**: `pgbench` runs against them. Each row becomes a Job.

While the runs go, a sampler records what the Postgres Pods use. One query then
puts each run's throughput beside the CPU and memory its Postgres used while
the benchmark was running:

```
OUTPUT-FROM-THE-E2E-RUN
```

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
psql -f lab-example.sql                           # two clusters, 500m and 2 CPUs; a run against each
psql -v namespace=regression-lab -f clusters.sql
```

Wait until both clusters are healthy:

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

## Limits

- **Runs on one cluster take turns.** `pgbench -i` rebuilds its tables, so
  `launch.sql` won't start a run while another run on the same cluster is
  unfinished. Launch again once it's done.
- **Termination messages are capped at 4 KiB.** That's enough for the JSON
  result, or the tail of an error. Reading whole logs from SQL is #105.
- **It acts as the gateway.** `rbac.yaml` lets the gateway create Clusters and
  Jobs in `regression-lab`. Every SQL role that can use the server acts as the
  gateway (#71), so any of them can do the same. Keep it a lab.
- **Shared clusters are noisy.** Pin the Pods to dedicated nodes before reading
  much into a few percent. The e2e suite runs this on a CI machine's kind
  cluster, so its numbers show the shape, not a benchmark.
