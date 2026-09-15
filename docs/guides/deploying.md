# Deploying the gateway

The gateway is a single stateless Go binary. It needs three things: cluster
credentials, a TLS server keypair, and a way for Postgres to reach it.

## As a Deployment

This is how the gateway is meant to run, and what every end-to-end gate
exercises. The manifests are in `deploy/k8s/`:

- `gateway-rbac.yaml` — the `axiom-system` namespace, the `axiom-gateway`
  ServiceAccount, and the ClusterRole that decides what it may read and write.
  This is the only thing that bounds what appears in SQL.
- `gateway-deployment.yaml` — the Deployment and a `NodePort` Service.

```sh
kubectl apply -f deploy/k8s/gateway-rbac.yaml
kubectl -n axiom-system create secret generic axiom-gateway-tls \
  --from-file=tls.crt=gateway.crt --from-file=tls.key=gateway.key
kubectl apply -f deploy/k8s/gateway-deployment.yaml
```

Raw YAML, deliberately: not Helm, not cert-manager, not an operator. Helm
templating hides the diffs that make a manifest reviewable, and cert-manager's
webhook adds startup latency and flakes in ephemeral clusters.

### Credentials

The Deployment passes no `-kubeconfig`, which is what selects
`rest.InClusterConfig()`. The gateway then runs on the projected ServiceAccount
token kubelet mounts and rotates in place, and holds no credential of its own.

There is nothing to configure here and nothing to rotate. If you find yourself
mounting a kubeconfig into the gateway, something has gone wrong: that replaces
a rotating, audience-bound token with a static credential, and an end-to-end
assertion fails on purpose if the flag reappears.

### Why `Recreate`

The Deployment uses `strategy: Recreate` rather than the default rolling
update. With one replica, a rolling update briefly runs two gateways, and the
Service can route a request to the one that is shutting down. That was observed
as a `ListKinds` that hung until its deadline rather than failing.

Two gateways would also mean two independent discovery and OpenAPI caches
answering alternate requests, which is not a state worth supporting for a
single-replica workload. A brief gap during a restart is the better trade: the
extension surfaces it as a connection error, or serves stale cache with a
`WARNING` for a watch-backed table.

### The readiness probe is TCP

The probe is a `tcpSocket` check, not Kubernetes' built-in gRPC probe. That
probe dials plaintext and cannot do TLS, and this server is TLS-only, so a gRPC
probe would never succeed and the Pod would never become ready.

TCP is a weaker signal but an honest one. The gateway builds its Kubernetes
client and loads its TLS material before it listens, so an open port does mean
the process got that far. Making the probe a real health check means adding a
TLS-capable probe binary to the distroless image.

## Reaching it from outside

Postgres runs outside the cluster. That is the point of Axiom, not a
limitation to work around, and it is why the exposure question is real.

**`NodePort`** is what the manifests use and what the gates rely on. It is
stable, it survives Pod replacement, and it holds long-lived HTTP/2 streams,
which the watch subscriptions need.

**A `LoadBalancer` or an Ingress with gRPC support** is the production answer.
Whatever you choose must terminate nothing: the extension verifies the
gateway's certificate itself, so a proxy that re-encrypts needs its own
certificate in `ca_cert`.

**Do not use `kubectl port-forward`** for anything but a one-off. It drops
long-lived streams, which is exactly what a watch-backed table depends on, and
it leaves orphaned subshells when a script exits.

### Certificate names

The extension verifies the gateway's certificate against the name it dialled.
The certificate's subject alternative names must therefore cover whatever is in
the `endpoint` server option, which for a `NodePort` is the node's name or
address, not `axiom-gateway.axiom-system.svc`.

The generator in `deploy/compose/certs/gen.sh` lists every name the local stack
might use, for exactly this reason. A name missing from the certificate is a
handshake failure, not a warning.

## The development loop

Rebuilding an image, loading it into a cluster and waiting for a rollout turns
a two-second Go change into a minute. For working *on* the gateway, run it as a
host process against your kubeconfig instead:

```sh
cd gateway
go run ./cmd/gateway \
  -kubeconfig ~/.kube/config \
  -listen 127.0.0.1:8443 \
  -tls-cert /tmp/certs/gateway.crt \
  -tls-key  /tmp/certs/gateway.key
```

Then point Postgres at `https://localhost:8443`. The certificate the compose
stack generates already carries `localhost` and `127.0.0.1` among its names, so
the same material works for both.

Two things to know about this mode:

- It authenticates as **you**, not as the gateway's ServiceAccount. Your
  kubeconfig almost certainly has more privilege than the ClusterRole, so a
  kind that works here can still be denied in the cluster. Confirm anything
  RBAC-shaped against a real deployment.
- `rest.InClusterConfig()` never runs, so the credential path the deployed
  gateway uses is not exercised. The end-to-end gates cover it; a host-process
  session does not.

Use it for iterating on gateway code. Use the Deployment for anything you
intend to believe.

## Operational notes

**Restarting** the gateway is safe at any time, and is required after an RBAC
change: access decisions are cached for the process lifetime. A kind removed
from the cluster needs no restart — resource lists expire after
`-discovery-ttl`, five minutes by default, and a re-import then reflects the
cluster.

```sh
kubectl -n axiom-system rollout restart deploy/axiom-gateway
```

Watch-backed tables go `DEGRADED` across the restart, keep serving cached rows
with a `WARNING`, and resume from their bookmark rather than relisting.

**Call counts** are readable from SQL, which is how to tell an on-demand scan
from a cache-served one without reading logs:

```sql
SELECT * FROM axiom_gateway_stats('prod');
```

The counters are per-process and reset when the Pod restarts.

**Watch state** is readable the same way:

```sql
SELECT * FROM axiom_watch_status();
```
