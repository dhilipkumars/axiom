# Initialize

Once the gateway is running and Postgres has Axiom installed and preloaded,
whichever way you [installed it](install/index.md), the steps are the same.
Run them in `psql` as a superuser.

The output on this page is from a CI run against a one-node kind cluster whose
gateway was limited to seven kinds, so its counts are small. With the shipped
RBAC the same import gives several dozen tables: 48 on a fresh kind cluster.

## 1. Create the extension

```sql
CREATE EXTENSION axiom;
```

If Postgres was not started with Axiom preloaded, this fails and says what to
do:

```
ERROR:  axiom must be loaded through shared_preload_libraries
DETAIL:  Add `shared_preload_libraries = 'axiom'` to postgresql.conf, restart
Postgres, then run CREATE EXTENSION axiom. ...
```

The published Postgres images preload it already. For a package or a source
build, see [Preload it, and restart](install/packages.md#2-preload-it-and-restart).

## 2. Point it at the gateway

A **server** is one cluster's gateway:

```sql
CREATE SERVER prod
  FOREIGN DATA WRAPPER axiom_fdw
  OPTIONS (
    endpoint 'https://gateway.example.internal:30443',
    ca_cert  '/etc/axiom/ca.crt'
  );

CREATE USER MAPPING FOR CURRENT_USER SERVER prod;
```

| Option | What goes there |
|---|---|
| `endpoint` | `https://` and an address Postgres can reach, which must be one of the names in the gateway's certificate. On kind, with Postgres on the `kind` network: `https://<cluster>-control-plane:30443`. |
| `ca_cert` | the gateway's CA, as a path **on the Postgres server's filesystem**, readable by the user Postgres runs as. In the Postgres image with `-v "$PWD/certs:/certs:ro"`: `/certs/ca.crt`. Leave it out for a certificate a public CA signed. |
| `rpc_timeout_secs` | optional, default `30`. Raise it if a whole-cluster import times out. |

The user mapping carries no options yet, so it takes no `OPTIONS` clause.
Every option is in the [FDW options reference](../generated/fdw-options.md).

## 3. Import the cluster's tables

```sql
CREATE SCHEMA k8s;
IMPORT FOREIGN SCHEMA k8s FROM SERVER prod INTO k8s;
```

This asks the gateway which kinds it may list, reads the cluster's OpenAPI
documents, and creates one foreign table per kind with typed columns.

Each table is named **`<group>_<plural>`**, with the core group spelled `core`:

| API group | Resource | Table |
|---|---|---|
| (core) | `pods` | `core_pods` |
| `apps` | `deployments` | `apps_deployments` |
| `metrics.k8s.io` | `pods` | `metrics_k8s_io_pods` |
| `postgresql.cnpg.io` | `clusters` | `postgresql_cnpg_io_clusters` |

A name depends only on its own group and resource, so it never changes because
something else was installed. Real clusters reuse plurals: metrics-server
serves `pods` beside the core ones, and CloudNativePG and Cluster API both
define `clusters`. In a group, `.` becomes `_` and `-` becomes `__`. A name
longer than Postgres's 63 bytes keeps its resource whole and shortens the
group, with a digest between them.

To import less, name the tables:

```sql
IMPORT FOREIGN SCHEMA k8s LIMIT TO (core_pods, apps_deployments) FROM SERVER prod INTO k8s;
```

`LIMIT TO` and `EXCEPT` take table names, not resources; a resource name
imports nothing, with a `WARNING` suggesting the table you meant.

A whole-cluster import on a large cluster can outlast the default timeout. If
it fails with `Cancelled: Timeout expired`, raise it rather than narrowing the
import:

```sql
ALTER SERVER prod OPTIONS (ADD rpc_timeout_secs '120');   -- SET, if it is already set
```

## 4. Short names (optional)

If you would rather type `pods` than `core_pods`, add short names. Each is a
view over its table:

```sql
--8<-- "docs/snippets/initialize/short-names.sql"
```

```
 status  | count 
---------+-------
 created |     5
(1 row)
```

- **The core group gets the bare plural.** `pods` is `core_pods` even with
  `metrics_k8s_io_pods` beside it.
- **Any other shared plural is skipped and reported**, never guessed. Choose
  one yourself:
  `SELECT axiom_create_short_name('k8s', 'clusters', 'postgresql_cnpg_io_clusters');`
- **An existing object is never replaced**, and a short name never moves once
  created.
- **Grant both.** The views use `security_invoker`, so a role needs `SELECT` on
  the view and on its table: `GRANT SELECT ON k8s.pods, k8s.core_pods TO app;`.

## 5. Check it

**The extension's version:**

```sql
--8<-- "docs/snippets/initialize/version.sql"
```

```
 axiom_version 
---------------
 0.2.0
(1 row)
```

**The background worker is running.** It keeps the stream to each gateway and
fills the cache. It exists only when Axiom was preloaded, so a row here
confirms the preload worked:

```sql
--8<-- "docs/snippets/initialize/worker.sql"
```

```
 pid |     backend_type     |         backend_start         
-----+----------------------+-------------------------------
  68 | axiom gateway pinger | 2026-10-05 02:56:43.114936+00
(1 row)
```

**The gateway is reachable, and has done work.** `axiom_gateway_stats` raises
an error for a gateway it cannot reach, rather than returning zeros:

```sql
--8<-- "docs/snippets/initialize/gateway-stats.sql"
```

```
    gateway_started     | list_calls | openapi_fetches | access_reviews 
------------------------+------------+-----------------+----------------
 2026-10-05 02:56:50+00 |          0 |               4 |              7
(1 row)
```

**The import created tables, and no Secrets.** The count depends on the
cluster and on what the gateway may read; this run's gateway served seven
kinds:

```sql
--8<-- "docs/snippets/initialize/tables.sql"
```

```
 tables | core | secrets 
--------+------+---------
      7 |    3 |       0
(1 row)
```

`\d k8s.core_pods` shows the columns discovery chose. Every table has the same
universal columns, then the kind's own top-level fields, then `raw` with the
whole object.

## 6. A first query

```sql
--8<-- "docs/snippets/initialize/first-query.sql"
```

```
                      name                       |  phase  |          node           |   creation_timestamp   
-------------------------------------------------+---------+-------------------------+------------------------
 coredns-7d764666f9-fdvfk                        | Running | axiom-e2e-control-plane | 2026-10-05 02:49:01+00
 coredns-7d764666f9-mb5fn                        | Running | axiom-e2e-control-plane | 2026-10-05 02:49:01+00
 etcd-axiom-e2e-control-plane                    | Running | axiom-e2e-control-plane | 2026-10-05 02:48:55+00
 kindnet-cvs68                                   | Running | axiom-e2e-control-plane | 2026-10-05 02:49:01+00
 kube-apiserver-axiom-e2e-control-plane          | Running | axiom-e2e-control-plane | 2026-10-05 02:48:55+00
 kube-controller-manager-axiom-e2e-control-plane | Running | axiom-e2e-control-plane | 2026-10-05 02:48:55+00
 kube-proxy-htwmm                                | Running | axiom-e2e-control-plane | 2026-10-05 02:49:01+00
 kube-scheduler-axiom-e2e-control-plane          | Running | axiom-e2e-control-plane | 2026-10-05 02:48:55+00
 metrics-server-6795649cdf-6hpnb                 | Running | axiom-e2e-control-plane | 2026-10-05 02:55:28+00
(9 rows)
```

`kubectl -n kube-system get pods` shows the same pods: the same data by a
different route. [Examples](examples/index.md) goes further.

## A kind is missing

Foreign tables are catalog objects. Changing the gateway's RBAC changes what it
offers, but not the tables that already exist. To see what it offers now,
import into a scratch schema:

```sql
CREATE SCHEMA probe;
IMPORT FOREIGN SCHEMA k8s FROM SERVER prod INTO probe;
SELECT foreign_table_name FROM information_schema.foreign_tables
 WHERE foreign_table_schema = 'probe' ORDER BY 1;
DROP SCHEMA probe CASCADE;
```

If the kind is absent there, the cause is RBAC, not the import:
[grant it](install/rbac.md), then import again. A kind removed from the
cluster drops out of a re-import within five minutes, when the gateway's
resource list expires.

## Refreshing an import

To pick up new kinds, or new columns after a cluster upgrade, drop the schema
and import again. Short names are views over the tables, so create them again
afterwards:

```sql
DROP SCHEMA k8s CASCADE;
CREATE SCHEMA k8s;
IMPORT FOREIGN SCHEMA k8s FROM SERVER prod INTO k8s;
SELECT * FROM axiom_create_short_names('k8s');
```

`CASCADE` also drops views of your own built on these tables, so keep their
definitions in a script.

## Removing it

```sql
DROP EXTENSION axiom CASCADE;   -- the servers, mappings and foreign tables with it
```

Then remove `axiom` from `shared_preload_libraries` and restart, and remove the
package or files.
