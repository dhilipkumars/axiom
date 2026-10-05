---
description: "Run Axiom, the Kubernetes foreign data wrapper for PostgreSQL, on a local kind cluster with one script, and query Kubernetes from SQL in minutes."
---

# Quick start

One script brings up everything on your machine and runs a first query:

- a [kind](https://kind.sigs.k8s.io) cluster named `axiom-quickstart`;
- the gateway in it, with a fresh TLS keypair;
- Postgres 17 with Axiom, in Docker, connected to the gateway;
- the cluster's tables, imported, with short names.

It takes a few minutes, and needs Docker, `kind`, `kubectl` and `curl`
([prerequisites](prerequisites.md)).

## Run it

```sh
curl -fsSL https://github.com/dhilipkumars/axiom/releases/latest/download/quickstart.sh | bash
```

Or download it, read it, and run it, which is worth doing with any script from
the internet:

```sh
curl -fsSLO https://github.com/dhilipkumars/axiom/releases/latest/download/quickstart.sh
curl -fsSLO https://github.com/dhilipkumars/axiom/releases/latest/download/quickstart.sh.sha256
sha256sum -c quickstart.sh.sha256            # macOS: shasum -a 256 -c quickstart.sh.sha256
less quickstart.sh
bash quickstart.sh
```

The script belongs to its release: it installs that release's images and
manifests, never whatever is on `main`. To pin a version, download it from that
release's page instead of `latest`.

## What you see

```
Axiom 0.2.0, Postgres 17, kind cluster 'axiom-quickstart'

==> kind cluster
created axiom-quickstart

==> TLS keypair for the gateway
written to ~/.axiom-quickstart/certs

==> gateway (ghcr.io/dhilipkumars/axiom-gateway:v0.2.0)
running in namespace axiom-system, NodePort 30443

==> Postgres 17 with Axiom (ghcr.io/dhilipkumars/axiom-postgres:0.2.0-pg17)
running as container axiom-quickstart-pg

==> connect Postgres to the gateway, and import the cluster
axiom 0.2.0, 48 tables imported into schema k8s

==> a first query: the cluster's kube-system pods, from SQL
SELECT name, phase, node FROM k8s.pods WHERE namespace = 'kube-system' ORDER BY name;

                          name                          |  phase  |              node              
--------------------------------------------------------+---------+--------------------------------
 coredns-559f6c778d-dl5f7                               | Running | axiom-quickstart-control-plane
 coredns-559f6c778d-p744z                               | Running | axiom-quickstart-control-plane
 etcd-axiom-quickstart-control-plane                    | Running | axiom-quickstart-control-plane
 kindnet-m7vcv                                          | Running | axiom-quickstart-control-plane
 kube-apiserver-axiom-quickstart-control-plane          | Running | axiom-quickstart-control-plane
 kube-controller-manager-axiom-quickstart-control-plane | Running | axiom-quickstart-control-plane
 kube-proxy-h5mkr                                       | Running | axiom-quickstart-control-plane
 kube-scheduler-axiom-quickstart-control-plane          | Running | axiom-quickstart-control-plane
(8 rows)


==> ready
Open psql:     docker exec -it axiom-quickstart-pg psql -U postgres
           or: psql postgresql://postgres:axiom@127.0.0.1:55432/postgres
Examples:      https://dhilipkumars.github.io/axiom/guides/examples/
Remove it all: bash quickstart.sh down    (piped: curl ... | bash -s down)
```

## Then

Open `psql` and query. Every example on this site works here:

```sh
docker exec -it axiom-quickstart-pg psql -U postgres
```

```sql
SELECT namespace, name, phase FROM k8s.pods ORDER BY 1, 2;
SELECT * FROM axiom_gateway_stats('prod');
```

Try the [examples](examples/index.md) next.

## Options

Set these before running it:

| Variable | Default | |
|---|---|---|
| `AXIOM_PG` | `17` | Postgres major: `16`, `17` or `18` |
| `AXIOM_CLUSTER` | `axiom-quickstart` | kind cluster name |
| `AXIOM_PORT` | `55432` | `psql` port on `127.0.0.1`; `0` publishes none |

It is safe to run again, and it never touches a kind cluster it did not create:
if a cluster with its name already exists and was not made by this script, it
stops and says so.

## Clean up

```sh
bash quickstart.sh down
# or, if you piped it:
curl -fsSL https://github.com/dhilipkumars/axiom/releases/latest/download/quickstart.sh | bash -s down
```

Removes the Postgres container, the kind cluster (only if the script created
it), and the TLS keypair in `~/.axiom-quickstart`.

## What it did

Nothing in the script is special: it is the [install](install/index.md) and
[initialize](initialize.md) steps, scripted for kind. Postgres joins the `kind`
Docker network and dials the gateway's NodePort by the node's container name,
so no port is opened on the cluster. On any other cluster, follow those pages
instead.
