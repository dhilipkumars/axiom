# Examples

Three small programs that use what only Axiom can do: write to the cluster
from SQL, get a live change feed in Postgres, and join cluster state to tables
of your own. Each is a handful of SQL files and a README, and each runs in the
e2e suite (`e2e/examples_test.sh`) exactly as shipped.

| Example | What it shows |
|---|---|
| [`sql-operator/`](sql-operator/) | A Kubernetes operator whose whole reconcile step is one `UPDATE`, driven by a table you own, woken by `NOTIFY` and kept correct by a sweep. |
| [`deploy-timeline/`](deploy-timeline/) | Everything that happened to a Helm release — Deployment, ReplicaSet, Pod, scheduling, image pull, readiness — on one timeline, from one query. |
| [`regression-lab/`](regression-lab/) | A Postgres benchmark matrix as a table: one `INSERT` starts a `pgbench` Job per row, and one query reads the results back from the cluster. |

They assume a working Axiom setup: the extension installed, a gateway it can
reach, and a server created with `CREATE SERVER`. The
[getting-started guide](https://dhilipkumars.github.io/axiom/guides/getting-started/)
sets that up on a kind cluster in about ten minutes. Each example takes the
server's name as a psql variable (`-v server=...`) and creates its own schema,
so none of them touches your other tables.
