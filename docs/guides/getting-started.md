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

**This walkthrough is written for [kind](https://kind.sigs.k8s.io).** Not
because Axiom needs it — the gateway runs on any cluster — but because of how
Postgres reaches the gateway here. Every kind cluster attaches to one Docker
network called `kind`, so running Postgres on that network lets it dial the
node container directly, and no port mapping or ingress is needed. On another
cluster you have to expose the gateway some other way first;
[Deploying the gateway](deploying.md) covers the choices, and step 5 is where
the endpoint changes.

Create one, or reuse a cluster you already have:

```sh
CLUSTER=axiom
kind get clusters | grep -qx "$CLUSTER" || kind create cluster --name "$CLUSTER"

NODE="${CLUSTER}-control-plane"
kubectl config use-context "kind-${CLUSTER}"
kubectl wait --for=condition=Ready node --all --timeout=180s
```

`$NODE` is the cluster's node container, and the rest of this guide uses it:
it goes in the certificate, and it is the host Postgres dials. **Keep these
two variables set for the whole walkthrough** — a new shell means setting them
again.

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
    printf 'subjectAltName=DNS:$NODE,DNS:axiom-gateway.axiom-system.svc,DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth\n' > san.cnf
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

`$NODE` is expanded by your shell before the container sees it, so the
certificate names your cluster's node. Check it if you changed `CLUSTER`:

```sh
docker run --rm -v "$PWD/certs:/certs:ro" alpine/openssl:3.3.3 \
  x509 -in /certs/gateway.crt -noout -ext subjectAltName
```

If you generate the keypair another way, the requirements are **named-curve EC
or RSA keys, and SHA-256 signatures**. The subject alternative names must cover
the name Postgres dials. rustls verifies the SAN against that name, and a miss
is a handshake failure, not a warning.

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

The manifest pulls `ghcr.io/dhilipkumars/axiom-gateway:latest`, which follows
releases. To pin a version instead:

```sh
kubectl -n axiom-system set image deploy/axiom-gateway \
  gateway=ghcr.io/dhilipkumars/axiom-gateway:v0.1.1
```

[Releasing](../RELEASING.md) explains what each tag means.

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
kubectl patch clusterrole axiom-gateway --type=json -p '[{
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
  ghcr.io/dhilipkumars/axiom-postgres:latest-pg17
```

Three things in that command matter:

- **`--network kind`** puts Postgres on the same Docker network as the cluster
  node, so it can reach the gateway's `NodePort` at `$NODE:30443` without any
  port mapping on the cluster. Every kind cluster uses this one network,
  whatever the cluster is called.
- **`-v "$PWD/certs:/certs:ro"`** because `ca_cert` below is a path on the
  *Postgres server's* filesystem, read by the backend process. Inside this
  container that is `/certs/ca.crt`.
- **`-p 55432:5432`** is only for your own `psql`; nothing in this guide needs
  it. Use `docker exec -it axiom-postgres psql -U postgres` instead if you
  prefer.

The image sets `shared_preload_libraries = 'axiom'` itself. That is not a
convenience: Axiom registers `PGC_POSTMASTER` GUCs, so without preloading
`CREATE EXTENSION` fails outright rather than running with the cache disabled.

`axiom-postgres` is published per major — `latest-pg16`, `latest-pg17`,
`latest-pg18` — each following the newest release for that major. Swap the tag
to match the Postgres you want, or pin a version like `0.1.1-pg17` if you would
rather choose when to move.

## 5. Install and connect

The endpoint has to name your cluster's node, so run this from the shell that
has `$NODE` set rather than typing it into `psql` — the heredoc is unquoted, so
the shell substitutes it before Postgres sees it:

```sh
docker exec -i axiom-postgres psql -U postgres -v ON_ERROR_STOP=1 <<SQL
CREATE EXTENSION axiom;
SELECT axiom_version();

CREATE SERVER prod
  FOREIGN DATA WRAPPER axiom_fdw
  OPTIONS (
    endpoint 'https://${NODE}:30443',
    ca_cert  '/certs/ca.crt'
  );

CREATE USER MAPPING FOR CURRENT_USER SERVER prod;
SQL
```

If you would rather work interactively from here on, the rest of this guide is
plain SQL with nothing to substitute:

```sh
docker exec -it axiom-postgres psql -U postgres
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

The guide above runs Postgres in a container. If you already have one, install
the extension into it instead — no toolchain, no rebuild. Three routes, best
first.

### A package, if this machine uses apt or dnf

Releases publish a `.deb` and an `.rpm` per major and architecture. Prefer
these: they place the files, they can be removed again, and — the part that
matters — **they refuse to install on a system whose glibc is too old to run
the library**, instead of letting you discover it when Postgres will not
restart.

```sh
V=0.1.1; PG=17
BASE=https://github.com/dhilipkumars/axiom/releases/download/v$V

# Debian, Ubuntu
ARCH=$(dpkg --print-architecture)
curl -fsSLO "$BASE/postgresql-$PG-axiom_$V-1_$ARCH.deb"
sudo apt install "./postgresql-$PG-axiom_$V-1_$ARCH.deb"

# RHEL, Rocky, Alma 9 — needs PGDG's repo for postgresql$PG-server
curl -fsSLO "$BASE/axiom_$PG-$V-1.el9.$(uname -m).rpm"
sudo dnf install "./axiom_$PG-$V-1.el9.$(uname -m).rpm"
```

The Debian package installs under `/usr/lib/postgresql/$PG`, the RPM under
`/usr/pgsql-$PG`, each matching what that distribution's Postgres expects.
Neither owns those directories, so neither conflicts with the server package.

A package cannot set `shared_preload_libraries` for you, and Axiom will not
load without it. Continue at [Preload, then connect](#preload-then-connect) —
not at step 5, which is written for the container this guide starts and uses
`docker exec` and a CA path inside it.

### A tarball, anywhere else

Releases publish a tarball per Postgres major and architecture, from `v0.1.1`
onward — **v0.1.0 predates them and has none**. Take `V` from the
[releases page](https://github.com/dhilipkumars/axiom/releases) rather than
copying the version below.

```sh
V=0.1.1; PG=17; ARCH=$(uname -m | sed -e s/x86_64/amd64/ -e s/aarch64/arm64/)
BASE=https://github.com/dhilipkumars/axiom/releases/download/v$V
curl -fsSLO "$BASE/axiom-$V-pg$PG-linux-$ARCH.tar.gz"
curl -fsSLO "$BASE/axiom-$V-pg$PG-linux-$ARCH.tar.gz.sha256"
sha256sum -c "axiom-$V-pg$PG-linux-$ARCH.tar.gz.sha256"   # macOS: shasum -a 256 -c
tar -xzf "axiom-$V-pg$PG-linux-$ARCH.tar.gz"
```

The tarball holds `axiom.so` and the extension's control and SQL files, under
the paths a Debian-packaged Postgres uses. Unpack them over your installation:

```sh
sudo cp -r axiom-$V-pg$PG-linux-$ARCH/usr/. /usr/
```

If your Postgres does not use those paths — a source build, or a non-Debian
package — place the two pieces where `pg_config` says they belong. This is
still Linux only; see the limits below.

```sh
sudo cp axiom-$V-pg$PG-linux-$ARCH/usr/lib/postgresql/$PG/lib/axiom.so \
  "$(pg_config --pkglibdir)/"
sudo cp axiom-$V-pg$PG-linux-$ARCH/usr/share/postgresql/$PG/extension/axiom* \
  "$(pg_config --sharedir)/extension/"
```

### Building from source, as a last resort

Only if no artifact matches your platform. This needs a Rust toolchain and
`cargo-pgrx` matching the `pg_config` of the server you are installing into,
and it is the one part of this guide that needs a checkout:

```sh
git clone https://github.com/dhilipkumars/axiom.git
cd axiom
cargo install cargo-pgrx --version 0.19.2 --locked
cargo pgrx init --pg17 "$(which pg_config)"
cd extension && cargo pgrx install --release --no-default-features --features pg17
```

`cargo pgrx install` writes into the directories `pg_config` reports, so it
needs permission to do that — run it as a user who has it, or with `sudo -E`
so the toolchain stays on `PATH`.

**Three places name the major and all must agree**: `--pg17` on
`cargo pgrx init`, `--features pg17`, and the `pg_config` you point at. Change
one for a different major and change all three, or the build fails in a way
that does not name the cause.

### Preload, then connect

However you placed the files — package, tarball or source build — the
remaining steps are the same.

Preload the library and restart. **This is not optional**: Axiom registers
`PGC_POSTMASTER` settings, so without it `CREATE EXTENSION` fails outright
rather than running with the cache disabled.

```ini
# postgresql.conf — append to any existing list rather than replacing it
shared_preload_libraries = 'axiom'
```

```sql
CREATE EXTENSION axiom;
SELECT axiom_version();
```

From here you still need a gateway, and the server and user mapping work the
same way — but **do not copy step 5 verbatim**. It is written for the Postgres
container this guide starts, and three of its details are specific to that:

| Step 5 says | On your own Postgres |
|---|---|
| `docker exec -it axiom-postgres psql` | your usual `psql` |
| `ca_cert '/certs/ca.crt'` | wherever the CA sits **on the database server's filesystem**, readable by the user Postgres runs as |
| `endpoint 'https://axiom-control-plane:30443'` | an address your host can actually reach |

That last one is the real work, and it is not a documentation detail: the
guide's endpoint is a container name on kind's Docker network, which nothing
outside Docker resolves. A Postgres elsewhere needs the gateway exposed to it —
a NodePort on a routable node address, a LoadBalancer, or an ingress.
[Deploying the gateway](deploying.md) covers those choices and how each
interacts with the certificate's SANs.

So the server definition, in your own `psql`, is:

```sql
CREATE EXTENSION axiom;

CREATE SERVER prod
  FOREIGN DATA WRAPPER axiom_fdw
  OPTIONS (
    endpoint 'https://gateway.reachable.from.here:8443',
    ca_cert  '/path/on/this/server/ca.crt'   -- omit for a publicly-trusted CA
  );

CREATE USER MAPPING FOR CURRENT_USER SERVER prod;

CREATE SCHEMA k8s;
IMPORT FOREIGN SCHEMA k8s FROM SERVER prod INTO k8s;

SELECT name, namespace, phase FROM k8s.pods WHERE namespace = 'kube-system';
```

That is the whole path. [Import a schema](#6-import-a-schema) and
[Query](#7-query), earlier on this page, go into what the import does and how
the columns are chosen — they are the same SQL, so read them for the detail
rather than for another set of steps.

### What the artifacts do and do not cover

- **glibc 2.34 or newer** — Debian 12+, Ubuntu 22.04+, RHEL 9+ and their
  rebuilds. Older systems such as Debian bullseye and RHEL 8 cannot load the
  library. The failure is at load time, so Postgres refuses to start with the
  preload set; there is no silent half-working state. This is the floor the
  binary actually requires, which is lower than the glibc of the Debian
  bookworm image it is built on — the two are not the same number.

  **The `.deb` and `.rpm` check this for you** and refuse to install on a
  system below the floor, which is the main reason to prefer one over the
  tarball if your machine uses apt or dnf.
- **Linux, amd64 and arm64.** No macOS or Windows tarballs are published.
  Building from source is the only route there, with the caveat that it is not
  something this project tests: every gate runs on Linux.
- **Match the major exactly.** A `pg17` tarball is compiled against
  PostgreSQL 17's headers. Installing it beside a different major does not
  work and is not made to fail gracefully.
- **Managed Postgres cannot use these at all.** Axiom is not a trusted
  extension and needs `shared_preload_libraries`, so RDS, Cloud SQL and Aurora
  are out regardless of how the files are delivered.

## Tearing it down

If you installed into your own Postgres, none of this applies: drop the server
with `DROP SERVER prod CASCADE`, and remove the files you copied. The rest is
for the container walkthrough above.

```sh
docker rm -f axiom-postgres
kind delete cluster --name "$CLUSTER"
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
