# Setting up Axiom (for coding agents)

A procedure for an autonomous agent to bring up a working Axiom environment and
prove it works. Same result as [Getting started](getting-started.md), written
for a reader that cannot ask a follow-up question.

Every command is non-interactive and safe to re-run. Each step states how to
verify it before moving on. **Do not proceed past a failed verification** —
[Failures](#failures) lists the ones with unhelpful messages and what they
actually mean.

## What you are building

Three pieces:

1. a Kubernetes cluster (kind, created here if absent);
2. the **gateway**, a Deployment in that cluster holding the cluster
   credentials and exposing gRPC over TLS on NodePort 30443;
3. **Postgres**, a container with the Axiom extension preinstalled, on the same
   Docker network, querying the gateway.

Postgres joins the `kind` Docker network and dials the node by container name,
so no port mapping is needed on the cluster.

## Before you start

```sh
docker version --format '{{.Server.Version}}'
kubectl version --client -o json | head -5
kind version
uname -m
```

All three must be installed. Record `uname -m`:

- `arm64` or `aarch64` → **every `docker run` and `docker pull` below needs
  `--platform linux/amd64`**. The published images are amd64 only.
- `x86_64` → omit that flag.

The commands below include the flag. Remove it on x86_64, or leave it; Docker
accepts a matching platform.

## Step 1 — cluster

**This procedure requires [kind](https://kind.sigs.k8s.io).** Not because
Axiom needs it, but because Postgres reaches the gateway over kind's Docker
network in step 4. On any other cluster the gateway has to be exposed some
other way first, and this procedure does not cover that.

It creates a cluster named `axiom` and uses that name throughout. Do not
substitute an existing cluster with a different name unless you also change
every later use of `axiom-control-plane`.

```sh
kind get clusters | grep -qx axiom || kind create cluster --name axiom
```

Verify. The node is `NotReady` for a while after creation, so wait for it
rather than reading `get nodes` once:

```sh
kubectl --context kind-axiom wait --for=condition=Ready node --all --timeout=180s
kubectl --context kind-axiom get nodes
```

Expect one node named `axiom-control-plane`, `Ready`. That name is used
verbatim later; do not substitute a different one.

## Step 2 — TLS keypair

The gateway has no plaintext mode. Generate the keypair **inside this
container** — macOS ships LibreSSL, whose output the gateway rejects for three
different reasons.

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
```

Verify:

```sh
ls certs/
docker run --rm -v "$PWD/certs:/certs:ro" alpine/openssl:3.3.3 \
  x509 -in /certs/gateway.crt -noout -ext subjectAltName
```

Check it in the container too. `openssl x509 -ext` is OpenSSL 1.1.1 and later;
the `openssl` on macOS is LibreSSL, which does not have the flag and fails with
`unknown option -ext`.

Expect exactly `ca.crt`, `gateway.crt`, `gateway.key`, and a SAN list
containing `DNS:axiom-control-plane`. If that name is missing, the TLS
handshake in step 5 fails; regenerate rather than continuing.

## Step 3 — gateway

```sh
RAW=https://raw.githubusercontent.com/dhilipkumars/axiom/main/deploy/k8s

kubectl --context kind-axiom apply -f "$RAW/gateway-rbac.yaml"

kubectl --context kind-axiom -n axiom-system delete secret axiom-gateway-tls --ignore-not-found
kubectl --context kind-axiom -n axiom-system create secret generic axiom-gateway-tls \
  --from-file=tls.crt=certs/gateway.crt \
  --from-file=tls.key=certs/gateway.key

kubectl --context kind-axiom apply -f "$RAW/gateway-deployment.yaml"
kubectl --context kind-axiom -n axiom-system rollout status deploy/axiom-gateway --timeout=180s
```

The `delete secret --ignore-not-found` before `create` is what makes this step
re-runnable; `create secret` alone fails on a second attempt.

Verify: `rollout status` exits 0 and prints
`deployment "axiom-gateway" successfully rolled out`. If it times out:

```sh
kubectl --context kind-axiom -n axiom-system get pods
kubectl --context kind-axiom -n axiom-system logs deploy/axiom-gateway --tail=30
```

## Step 4 — Postgres with Axiom preinstalled

```sh
docker rm -f axiom-postgres 2>/dev/null || true
docker run -d --name axiom-postgres --platform linux/amd64 \
  --network kind \
  -e POSTGRES_PASSWORD=axiom \
  -v "$PWD/certs:/certs:ro" \
  ghcr.io/dhilipkumars/axiom-postgres:development-pg17
```

Wait for readiness — **poll, do not sleep a fixed amount**:

```sh
for i in $(seq 1 90); do
  docker exec axiom-postgres pg_isready -U postgres >/dev/null 2>&1 && break
  sleep 2
done
docker exec axiom-postgres pg_isready -U postgres
```

Expect `/var/run/postgresql:5432 - accepting connections`.

Verify the extension is loadable, which is what the preload buys:

```sh
docker exec axiom-postgres psql -U postgres -tAc "SHOW shared_preload_libraries"
```

Expect `axiom`. Anything else means step 5 will fail fatally, not degrade.

## Step 5 — connect Postgres to the gateway

Use `docker exec -i` with a heredoc. **Never `-it`**: with no TTY attached it
either errors or hangs waiting for one.

```sh
docker exec -i axiom-postgres psql -U postgres -v ON_ERROR_STOP=1 <<'SQL'
CREATE EXTENSION IF NOT EXISTS axiom;
DROP SERVER IF EXISTS prod CASCADE;
CREATE SERVER prod
  FOREIGN DATA WRAPPER axiom_fdw
  OPTIONS (endpoint 'https://axiom-control-plane:30443', ca_cert '/certs/ca.crt');
CREATE USER MAPPING FOR CURRENT_USER SERVER prod;
CREATE SCHEMA IF NOT EXISTS k8s;
IMPORT FOREIGN SCHEMA k8s FROM SERVER prod INTO k8s;
SQL
```

`DROP SERVER IF EXISTS ... CASCADE` and the `IF NOT EXISTS` clauses make this
re-runnable. `ON_ERROR_STOP=1` matters: without it `psql` continues past a
failed statement and exits 0, and you would conclude the step succeeded.

Verify:

```sh
docker exec axiom-postgres psql -U postgres -tAc \
  "SELECT count(*) FROM information_schema.tables WHERE table_schema='k8s'"
```

Expect a number greater than 0.

## Step 6 — success criterion

```sh
docker exec axiom-postgres psql -U postgres -c \
  "SELECT name, namespace, phase, node FROM k8s.pods WHERE namespace='kube-system' ORDER BY name"
```

**The environment is working when this returns the cluster's `kube-system`
pods** — on a fresh kind cluster, rows including `etcd-axiom-control-plane` and
`kube-apiserver-axiom-control-plane`, all `Running`, all on node
`axiom-control-plane`. Compare against the cluster directly if you want a
second source:

```sh
kubectl --context kind-axiom -n kube-system get pods
```

The two must agree. They are the same data by different routes, which is the
point of the project.

## Failures

| Symptom | Cause | Action |
| --- | --- | --- |
| `no matching manifest for linux/arm64/v8` | images are amd64 only, you are on arm64 | add `--platform linux/amd64` |
| `denied` on `docker pull` | image is private or the tag does not exist | check the tag; do not retry with credentials |
| `FATAL: cannot create PGC_POSTMASTER variables after startup` | Axiom loaded without `shared_preload_libraries` | use the published image, or preload it |
| `CREATE EXTENSION` closes the connection | same as above | as above |
| TLS handshake failure on the first `IMPORT` | certificate SAN does not cover `axiom-control-plane` | regenerate the keypair, step 2 |
| `Cancelled: Timeout expired` on `IMPORT` | whole-cluster import exceeded `rpc_timeout_secs` | `ALTER SERVER prod OPTIONS (SET rpc_timeout_secs '120')` |
| `rollout status` times out | image pull or crash loop | `kubectl -n axiom-system logs deploy/axiom-gateway` |
| `psql` exits 0 but nothing was created | missing `ON_ERROR_STOP=1` | re-run with it |

## Teardown

Leaving a kind cluster running consumes memory indefinitely. Tear down unless
asked to keep it:

```sh
docker rm -f axiom-postgres
kind delete cluster --name axiom
rm -rf certs
```

## Constraints worth knowing before you suggest things

- **Axiom cannot be installed on managed Postgres.** It is not a trusted
  extension and needs `shared_preload_libraries`, so RDS, Cloud SQL and Aurora
  cannot run it. Do not suggest them.
- **There is no downloadable extension artifact yet.** Installing into an
  existing Postgres means building from source; the images are the supported
  path today.
- **Images are amd64 only**, so on arm64 everything runs under emulation.
- **What a query can reach is bounded by the gateway's RBAC**, not the SQL
  user's. To expose more kinds, change the ClusterRole and restart the gateway,
  then re-import — foreign tables are catalog objects and do not follow the
  change.
