# Examples

Three small programs that use what only Axiom can do: write to the cluster
from SQL, get a live change feed in Postgres, and join cluster state to tables
of your own. Each is a handful of SQL files and a README, and each runs in the
e2e suite (`e2e/examples_test.sh`) exactly as shipped.

| Example | What it shows |
|---|---|
| [`sql-operator/`](sql-operator/) | A Kubernetes operator whose whole reconcile step is one `UPDATE`, driven by a table you own, woken by `NOTIFY` and kept correct by a sweep. |
| [`deploy-timeline/`](deploy-timeline/) | Everything that happened to a Helm release — Deployment, ReplicaSet, Pod, scheduling, image pull, readiness — on one timeline, from one query. |
| [`regression-lab/`](regression-lab/) | Postgres clusters created by CloudNativePG from a table, `pgbench` Jobs launched against them from another, and one query that puts each run's throughput beside the CPU and memory its Postgres used while it ran. |

They assume a working Axiom setup: the extension installed, a gateway it can
reach, and a server created with `CREATE SERVER`. The
[getting-started guide](https://dhilipkumars.github.io/axiom/guides/getting-started/)
sets that up on a kind cluster in about ten minutes. The operator and the lab
take the server's name as a psql variable (`-v server=...`) and create their
own schema, `sqlop` and `lab`. The timeline reads tables from an ordinary
`IMPORT FOREIGN SCHEMA` into `k8s`, which its README shows.
