# Prerequisites

Axiom has two halves, and each needs something different.

## For the quick start

The [quick start](quick-start.md) runs everything on one machine, in Docker.

| Tool | Why | Check |
|---|---|---|
| Docker | runs the kind cluster and Postgres | `docker version` |
| [kind](https://kind.sigs.k8s.io) | a local Kubernetes cluster | `kind version` |
| `kubectl` | applies the gateway's manifests | `kubectl version --client` |
| `curl` | fetches the script and the manifests | `curl --version` |

About 2 GB of free memory, and nothing else listening on `127.0.0.1:55432`.
Linux and macOS both work, on amd64 or arm64.

## For a real installation

**A Kubernetes cluster** you can apply manifests to with `kubectl`: a namespace,
a Deployment, a Service, and cluster-scoped RBAC. The gateway runs there.

**A Postgres 16, 17 or 18** that you control, because Axiom has to be loaded
through `shared_preload_libraries` and Postgres restarted. That rules out
managed Postgres such as RDS, Cloud SQL and Aurora.
[Compatibility](../compatibility.md) lists the distributions and architectures.

**A network path from Postgres to the gateway.** Postgres always dials out to
the gateway, never the reverse, so the gateway has to be reachable from the
database host: a NodePort on a routable node address, a LoadBalancer, or an
ingress that passes gRPC through. [Install the
gateway](install/gateway.md#reaching-it-from-postgres) covers the choices.

**A TLS keypair for the gateway**, whose names cover the address Postgres dials.
The gateway has no plaintext mode.

## What you do not need

- No kubeconfig or Kubernetes credentials on the Postgres host.
- No inbound connectivity to Postgres from the cluster.
- No Rust or Go toolchain, unless you build from source.
