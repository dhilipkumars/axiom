# Getting started

This walks from nothing to a SQL query that returns real cluster data. It
assumes you have a Kubernetes cluster you can reach with `kubectl` and a
Postgres you can install an extension into.

If you only want to see it work, skip all of this and run the end-to-end
suite, which builds everything, creates a throwaway kind cluster and tears it
down again:

```sh
./e2e/run_all.sh
```

## 1. Run a gateway in the cluster

The gateway is the only piece that needs cluster credentials. It runs as a
Deployment with a ServiceAccount, and it needs a TLS server keypair because it
has no plaintext mode.

```sh
kubectl apply -f deploy/k8s/gateway-rbac.yaml

kubectl -n axiom-system create secret generic axiom-gateway-tls \
  --from-file=tls.crt=gateway.crt \
  --from-file=tls.key=gateway.key

kubectl -n axiom-system create configmap axiom-gateway-config \
  --from-literal=serve='pods,configmaps,deployments.apps'

kubectl apply -f deploy/k8s/gateway-deployment.yaml
```

The certificate's subject alternative names must cover the name Postgres will
dial. For a `NodePort`, that is the node's name or address, not the in-cluster
Service DNS. [Deploying the gateway](deploying.md) covers the exposure choices
and how they interact with the certificate.

Two things bound what the gateway will serve, and both apply:

- `-serve`, from the ConfigMap above, is the allowlist of kinds it offers.
- The ClusterRole in `deploy/k8s/gateway-rbac.yaml` is what the API server will
  actually permit. Discovery asks the API server which kinds this identity may
  list, so narrowing RBAC narrows what appears in SQL.

Keep the two in step. `-serve` that is wider than RBAC is harmless, just
ineffective; RBAC wider than `-serve` means privileges nothing uses.

## 2. Install the extension into Postgres

Axiom is a pgrx extension, built and installed like any other:

```sh
cd extension && cargo pgrx install --release --no-default-features --features pg16
```

Then, in the database you want to query from:

```sql
CREATE EXTENSION axiom;
SELECT axiom_version();
```

## 3. Point Postgres at the gateway

```sql
CREATE SERVER prod
  FOREIGN DATA WRAPPER axiom_fdw
  OPTIONS (
    endpoint 'https://node.example:30443',
    ca_cert  '/etc/postgresql/axiom-ca.crt'
  );

CREATE USER MAPPING FOR CURRENT_USER SERVER prod OPTIONS ();
```

`ca_cert` is a path on the Postgres **server's** filesystem, read by the
backend process, so it must be readable by the user Postgres runs as. Leaving
it out falls back to the host trust store, which is what you want if the
gateway's certificate is signed by a real CA.

Every accepted option is listed in the
[foreign data wrapper options](../generated/fdw-options.md) reference.

## 4. Import a schema

`IMPORT FOREIGN SCHEMA` asks the gateway what it serves, reads the cluster's
OpenAPI documents, and writes one foreign table per kind.

```sql
CREATE SCHEMA k8s;

-- Everything the gateway offers.
IMPORT FOREIGN SCHEMA k8s FROM SERVER prod INTO k8s;

-- Or one API group at a time. The schema name is the API group.
IMPORT FOREIGN SCHEMA "apps" FROM SERVER prod INTO k8s;
```

A whole-cluster import on a large cluster can take longer than the default
30-second `rpc_timeout_secs`, because it does one access review per kind on top
of the OpenAPI fetches. If an import fails with `Cancelled: Timeout expired`,
raise the timeout on the server rather than narrowing the import:

```sql
ALTER SERVER prod OPTIONS (SET rpc_timeout_secs '120');
```

## 5. Query

```sql
SELECT name, namespace, phase, node
  FROM k8s.pods
 WHERE namespace = 'kube-system'
 ORDER BY name;
```

`\d k8s.pods` shows what discovery chose. Every table gets the same universal
columns, plus the kind's own top-level fields, plus a `raw jsonb` column
holding the whole object. The [column reference](../generated/columns.md)
explains the rules.

## What to check when a kind is missing

Foreign tables are catalog objects. Changing the gateway's RBAC or its `-serve`
list changes what the gateway offers, but it does not change tables that
already exist, and nothing announces the drift. A kind that stopped being
served does not disappear from the catalog; its scans simply start failing.

There is no SQL call that lists what the gateway currently offers, so ask it
the way the import does, by importing into a scratch schema and looking at what
arrives:

```sql
CREATE SCHEMA probe;
IMPORT FOREIGN SCHEMA k8s FROM SERVER prod INTO probe;
SELECT table_name FROM information_schema.tables
 WHERE table_schema = 'probe' ORDER BY 1;
DROP SCHEMA probe CASCADE;
```

If the kind is absent there, the cause is `-serve` or RBAC rather than the
import. Fix whichever it is, restart the gateway, and re-import into the real
schema. A kind that has been *removed* from the cluster is the mirror case: it
stays on offer until the gateway restarts, because a group's resource list is
fetched once and never refreshed.

The gateway also logs which of the two bounds is narrowing at startup:

```sh
kubectl -n axiom-system logs deploy/axiom-gateway | grep "kubernetes client configured"
```
