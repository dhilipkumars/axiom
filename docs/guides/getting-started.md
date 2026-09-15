# Getting started

This walks from nothing to a SQL query that returns real cluster data, using
kind for the cluster. It assumes Docker, `kubectl`, `kind` and a Postgres you
can install an extension into.

Every command here has been run end to end. If one does not work, that is a
bug worth reporting.

If you only want to see it work, skip all of this and run the end-to-end
suite, which builds everything, creates a throwaway kind cluster and tears it
down again:

```sh
./e2e/run_all.sh
```

## 1. Make the node port reachable

Postgres runs outside the cluster and dials the gateway's `NodePort`, so that
port has to be reachable from wherever Postgres is. On a managed cluster the
node address usually is. **On kind it is not**, unless the cluster was created
with a port mapping, and adding one afterwards means recreating the cluster:

```sh
cat <<'EOF' | kind create cluster --name axiom --config -
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
  - role: control-plane
    extraPortMappings:
      - containerPort: 30443
        hostPort: 30443
        protocol: TCP
EOF
```

With that mapping the gateway is reachable at `https://localhost:30443`, which
is the endpoint used throughout this guide.

## 2. Generate a TLS keypair

The gateway has no plaintext mode, so it needs a server certificate before it
will start. The repository's generator produces one with every name the local
stack might use:

```sh
mkdir -p certs
docker run --rm --entrypoint /bin/sh \
  -e E2E_KIND_CLUSTER=axiom \
  -v "$PWD/certs:/certs" \
  -v "$PWD/deploy/compose/certs/gen.sh:/gen.sh:ro" \
  alpine/openssl:3.3.3 -c "/gen.sh && chown $(id -u):$(id -g) /certs/*"
ls certs/          # ca.crt  gateway.crt  gateway.key
```

Three details in that command, each of which breaks the next step if dropped:

- **`--entrypoint`**, because the image runs `openssl` by default and would
  otherwise pass the script to it as an argument.
- **`E2E_KIND_CLUSTER`**, because the node name goes into the certificate and
  defaults to the end-to-end suite's cluster. It must match the cluster created
  above, or the certificate names a node that does not exist.
- **`chown`**, because the script gives the private key to uid 65532, the user
  the gateway runs as, with mode 0600. On Linux that leaves it unreadable by
  the account running `kubectl` in the next step, which fails with `permission
  denied`. Docker Desktop remaps ownership and hides this, so it bites on Linux
  only.

The certificate carries `localhost`, `127.0.0.1`, the in-cluster Service names
and the kind node name, so it covers every way this guide reaches the gateway.

Run it in that container rather than on the host. **macOS ships LibreSSL, and
certificates it generates are rejected** in three different ways that name
nothing useful:

| What LibreSSL does | How it fails |
| --- | --- |
| EC keys with explicit curve parameters | gateway crash-loops, `x509: invalid ECDSA parameters` |
| signs with SHA-1 by default | `UnsupportedSignatureAlgorithmContext` from the extension |
| CA key with explicit parameters | `UnsupportedSignatureAlgorithmForPublicKeyContext` |

If you generate the keypair yourself, the requirements are **named-curve EC or
RSA keys, and SHA-256 signatures**. The subject alternative names must cover
the name Postgres will dial, which for a `NodePort` is the node address or
`localhost`, not the in-cluster Service DNS.

## 3. Run the gateway in the cluster

The gateway is the only piece that needs cluster credentials. It runs as a
Deployment with a ServiceAccount and the projected token kubelet mounts.

```sh
kubectl apply -f deploy/k8s/gateway-rbac.yaml

kubectl -n axiom-system create secret generic axiom-gateway-tls \
  --from-file=tls.crt=certs/gateway.crt \
  --from-file=tls.key=certs/gateway.key

kubectl apply -f deploy/k8s/gateway-deployment.yaml
kubectl -n axiom-system rollout status deploy/axiom-gateway
```

`deploy/k8s/gateway-deployment.yaml` pulls
`ghcr.io/dhilipkumars/axiom-gateway:development` by default. If you are following this
from a fork that publishes under a different owner, patch the manifest's
`image:` field to that owner first. If you are iterating on a local gateway
build and want kind to run that instead, tag it with the same reference,
side-load it, and apply a local `IfNotPresent` override so the first Pod does
not try to pull before the patch lands:

```sh
docker build -f gateway/Dockerfile -t ghcr.io/dhilipkumars/axiom-gateway:development .
kind load docker-image ghcr.io/dhilipkumars/axiom-gateway:development --name axiom
sed '0,/imagePullPolicy: Always/s//imagePullPolicy: IfNotPresent/' \
  deploy/k8s/gateway-deployment.yaml | kubectl apply -f -
kubectl -n axiom-system rollout status deploy/axiom-gateway
```

[Deploying the gateway](deploying.md) covers the exposure choices and how they
interact with the certificate.

## What the gateway can see

**RBAC decides, and nothing else needs configuring.** Discovery asks the API
server which kinds this ServiceAccount may list, and offers exactly those. To
change what appears in SQL, change the ClusterRole.

That is the bound worth having, because the API server enforces it. A kind you
have not granted cannot be read even if something asks for it.

The bundled ClusterRole in `deploy/k8s/gateway-rbac.yaml` is a **starting
point, not a recommendation**. It grants Pods, ConfigMaps, and an example
custom resource. To add Deployments, which later examples on this site use:

```sh
kubectl patch clusterrole axiom-gateway-read --type=json -p '[{
  "op": "add", "path": "/rules/-",
  "value": {"apiGroups": ["apps"], "resources": ["deployments"],
            "verbs": ["get", "list", "watch"]}
}]'

kubectl -n axiom-system rollout restart deploy/axiom-gateway
```

Restart the gateway after an RBAC change: it caches what it may read, so the
new grant appears on the next start. Then re-run `IMPORT FOREIGN SCHEMA`, since
foreign tables are catalog objects and do not follow the change on their own.

Grant only the verbs you want available. A kind granted `get`, `list` and
`watch` is readable and cacheable but not writable, and the generated table
reflects that.

## 4. Install the extension into Postgres

Axiom is a pgrx extension, built and installed like any other:

```sh
cd extension && cargo pgrx install --release --no-default-features --features pg16
```

**The feature must match the Postgres you are building against.** Axiom
supports 16 through the latest major, so use `pg16`, `pg17` or `pg18` to match
the `pg_config` that `cargo pgrx init` was pointed at. A mismatch fails with a
pgrx error that does not make the cause obvious.

Then, in the database you want to query from:

```sql
CREATE EXTENSION axiom;
SELECT axiom_version();
```

## 5. Point Postgres at the gateway

```sql
CREATE SERVER prod
  FOREIGN DATA WRAPPER axiom_fdw
  OPTIONS (
    endpoint 'https://localhost:30443',
    ca_cert  '/absolute/path/to/certs/ca.crt'
  );

CREATE USER MAPPING FOR CURRENT_USER SERVER prod;
```

The user mapping carries no options today, so it takes no `OPTIONS` clause.
Postgres rejects an empty `OPTIONS ()` with a syntax error. Per-caller
credentials are on the roadmap, and are what will eventually give the mapping
something to hold.

`ca_cert` is a path on the Postgres **server's** filesystem, read by the
backend process, so it must be readable by the user Postgres runs as. Leaving
it out falls back to the host trust store, which is what you want if the
gateway's certificate is signed by a real CA.

Every accepted option is listed in the
[foreign data wrapper options](../generated/fdw-options.md) reference.

## 6. Import a schema

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

## 7. Query

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

Foreign tables are catalog objects. Changing the gateway's RBAC changes what it
offers, but it does not change tables that already exist, and nothing announces
the drift. A kind that stopped being served does not disappear from the
catalog; its scans simply start failing.

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

If the kind is absent there, the import is not the cause: it is RBAC. Grant
it, restart the gateway so it re-reads what it may access, and re-import into
the real schema.

A kind that has been *removed* from the cluster is the mirror case, and needs
no intervention: resource lists expire after `-discovery-ttl`, five minutes by
default, so a re-import a few minutes later reflects the cluster.

If the grant looks right and the kind is still missing, the gateway logs what
it resolved at startup:

```sh
kubectl -n axiom-system logs deploy/axiom-gateway | grep "kubernetes client configured"
```
