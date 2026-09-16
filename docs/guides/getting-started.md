# Getting started

From nothing to a SQL query that returns real cluster data. **Nothing here is
built from source**: it uses published images, and manifests applied straight
from a URL. You need Docker, `kubectl` and `kind`.

Every command has been run end to end, in this order. If one does not work,
that is a bug worth reporting.

!!! note "On Apple Silicon"

    The images are `linux/amd64` only for now, so `docker` needs
    `--platform linux/amd64` or it fails with *no matching manifest for
    linux/arm64/v8*. It runs under emulation. Every `docker` command below
    already carries the flag; drop it on an amd64 machine if you prefer.

## 1. A cluster

Any cluster works. If you do not have one:

```sh
kind create cluster --name axiom
```

Note the node's container name, `axiom-control-plane`. Postgres dials the
gateway through it, which is why this guide never needs a port mapping: both
containers sit on the `kind` Docker network and talk directly.

## 2. A TLS keypair

The gateway has no plaintext mode, so it needs a server certificate before it
will start.

```sh
mkdir -p certs
docker run --rm -v "$PWD/certs:/certs" -w /certs \
  --entrypoint /bin/sh alpine/openssl:3.3.3 -c "
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
      -days 365 -subj '/CN=axiom-dev-ca' -keyout ca.key -out ca.crt
    openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
      -subj '/CN=gateway' -keyout gateway.key -out gateway.csr
    printf 'subjectAltName=DNS:axiom-control-plane,DNS:axiom-gateway.axiom-system.svc,DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth\n' > san.cnf
    openssl x509 -req -in gateway.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
      -days 365 -extfile san.cnf -out gateway.crt
    rm -f gateway.csr san.cnf ca.srl ca.key
    chown $(id -u):$(id -g) ca.crt gateway.crt gateway.key
  "
ls certs/          # ca.crt  gateway.crt  gateway.key
```

**Generate it in that container, not on your Mac.** macOS ships LibreSSL, and
certificates it produces are rejected in three ways that name nothing useful:

| What LibreSSL does | How it fails |
| --- | --- |
| EC keys with explicit curve parameters | gateway crash-loops, `x509: invalid ECDSA parameters` |
| signs with SHA-1 by default | `UnsupportedSignatureAlgorithmContext` from the extension |
| CA key with explicit parameters | `UnsupportedSignatureAlgorithmForPublicKeyContext` |

If you generate the keypair another way, the requirements are **named-curve EC
or RSA keys, and SHA-256 signatures**. The subject alternative names must cover
the name Postgres dials — `axiom-control-plane` here. rustls verifies the SAN
against that name, and a miss is a handshake failure, not a warning.

## 3. The gateway, in the cluster

The gateway is the only piece that needs cluster credentials. It runs as a
Deployment with a ServiceAccount and the projected token kubelet mounts. The
manifests are applied from the repository without cloning it:

```sh
RAW=https://raw.githubusercontent.com/dhilipkumars/axiom/main/deploy/k8s

kubectl apply -f "$RAW/gateway-rbac.yaml"

kubectl -n axiom-system create secret generic axiom-gateway-tls \
  --from-file=tls.crt=certs/gateway.crt \
  --from-file=tls.key=certs/gateway.key

kubectl apply -f "$RAW/gateway-deployment.yaml"
kubectl -n axiom-system rollout status deploy/axiom-gateway
```

That pulls `ghcr.io/dhilipkumars/axiom-gateway:development`, which is built
nightly from `main`. Releases publish a versioned tag; see
[Releasing](../RELEASING.md) for what each tag means.

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

## 4. Postgres, with Axiom already in it

Rather than installing the extension, run a Postgres image that has it:

```sh
docker run -d --name axiom-postgres --platform linux/amd64 \
  --network kind \
  -e POSTGRES_PASSWORD=axiom \
  -v "$PWD/certs:/certs:ro" \
  -p 55432:5432 \
  ghcr.io/dhilipkumars/axiom-postgres:development-pg17
```

Three things in that command matter:

- **`--network kind`** puts Postgres on the same Docker network as the cluster
  node, so it can reach the gateway's `NodePort` at `axiom-control-plane:30443`
  without any port mapping on the cluster.
- **`-v "$PWD/certs:/certs:ro"`** because `ca_cert` below is a path on the
  *Postgres server's* filesystem, read by the backend process. Inside this
  container that is `/certs/ca.crt`.
- **`-p 55432:5432`** is only for your own `psql`; nothing in this guide needs
  it. Use `docker exec -it axiom-postgres psql -U postgres` instead if you
  prefer.

The image sets `shared_preload_libraries = 'axiom'` itself. That is not a
convenience: Axiom registers `PGC_POSTMASTER` GUCs, so without preloading
`CREATE EXTENSION` fails outright rather than running with the cache disabled.

`axiom-postgres` is published per major — `-pg16`, `-pg17`, `-pg18`. Swap the
tag to match the Postgres you want.

## 5. Install and connect

```sh
docker exec -it axiom-postgres psql -U postgres
```

```sql
CREATE EXTENSION axiom;
SELECT axiom_version();

CREATE SERVER prod
  FOREIGN DATA WRAPPER axiom_fdw
  OPTIONS (
    endpoint 'https://axiom-control-plane:30443',
    ca_cert  '/certs/ca.crt'
  );

CREATE USER MAPPING FOR CURRENT_USER SERVER prod;
```

The user mapping carries no options today, so it takes no `OPTIONS` clause;
Postgres rejects an empty `OPTIONS ()` with a syntax error. Per-caller
credentials are on the roadmap, and are what will eventually give the mapping
something to hold.

Leaving `ca_cert` out falls back to the host trust store, which is what you
want when the gateway's certificate is signed by a real CA. Every accepted
option is in the [foreign data wrapper options](../generated/fdw-options.md)
reference.

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

## Installing into a Postgres you already run

This guide runs Postgres in a container because that is what can be done today
without a toolchain. Installing Axiom into an existing Postgres currently means
building the extension from source — see the repository's README.

Downloadable extension artifacts, so that no longer needs a Rust toolchain, are
the next thing on the roadmap. Note that Axiom cannot be installed on managed
Postgres at all: it is not a trusted extension and needs
`shared_preload_libraries`, neither of which RDS, Cloud SQL or Aurora permit.

## Tearing it down

```sh
docker rm -f axiom-postgres
kind delete cluster --name axiom
rm -rf certs
```

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
