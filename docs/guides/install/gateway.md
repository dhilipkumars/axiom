# Install the gateway

The gateway is a single stateless Go binary, run in the cluster as a
Deployment. It is the only part of Axiom that needs cluster credentials, and it
needs three things: its RBAC, a TLS keypair, and a way for Postgres to reach it.

A Helm chart is coming
([#116](https://github.com/dhilipkumars/axiom/issues/116)). Until then, the
gateway installs from two manifests in
[`deploy/k8s/`](https://github.com/dhilipkumars/axiom/tree/main/deploy/k8s):

- `gateway-rbac.yaml`: the `axiom-system` namespace, the `axiom-gateway`
  ServiceAccount, and the ClusterRoles that decide what it may read and write.
- `gateway-deployment.yaml`: the Deployment and a `NodePort` Service on port
  `30443`.

## 1. A TLS keypair

The gateway has no plaintext mode. Its certificate must name the address
Postgres will dial, because the extension verifies that name.

```sh
GATEWAY_HOST=gateway.example.internal     # the name Postgres will dial

mkdir -p certs
docker run --rm -v "$PWD/certs:/certs" -w /certs \
  --entrypoint /bin/sh alpine/openssl:3.3.3 -c "
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
      -days 365 -subj '/CN=axiom-ca' -keyout ca.key -out ca.crt
    openssl req -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
      -subj '/CN=gateway' -keyout gateway.key -out gateway.csr
    printf 'subjectAltName=DNS:$GATEWAY_HOST,DNS:axiom-gateway.axiom-system.svc\nextendedKeyUsage=serverAuth\n' > san.cnf
    openssl x509 -req -in gateway.csr -CA ca.crt -CAkey ca.key -CAcreateserial \
      -days 365 -extfile san.cnf -out gateway.crt
    rm -f gateway.csr san.cnf ca.srl ca.key
    chown $(id -u):$(id -g) ca.crt gateway.crt gateway.key
  "
```

Keep `ca.crt`: Postgres needs it to trust the gateway. A certificate from your
own CA works the same way, as long as it uses **named-curve EC or RSA keys and
SHA-256 signatures**.

!!! warning "Generate it in that container on macOS"

    macOS ships LibreSSL, whose certificates the gateway and extension reject:
    EC keys with explicit curve parameters crash-loop the gateway with
    `x509: invalid ECDSA parameters`, and SHA-1 signatures fail the handshake
    with `UnsupportedSignatureAlgorithmContext`.

## 2. Apply the manifests

Use the manifests from the release you are installing, so they match the
image:

```sh
V=0.2.0
RAW=https://raw.githubusercontent.com/dhilipkumars/axiom/v$V/deploy/k8s

kubectl apply -f "$RAW/gateway-rbac.yaml"
kubectl -n axiom-system create secret generic axiom-gateway-tls \
  --from-file=tls.crt=certs/gateway.crt --from-file=tls.key=certs/gateway.key
kubectl apply -f "$RAW/gateway-deployment.yaml"
kubectl -n axiom-system set image deploy/axiom-gateway \
  gateway=ghcr.io/dhilipkumars/axiom-gateway:v$V
kubectl -n axiom-system rollout status deploy/axiom-gateway
```

The manifest names `axiom-gateway:latest`, which follows releases; the
`set image` pins the version instead.

## Reaching it from Postgres

Postgres runs outside the cluster. That is the point of Axiom, and it is why
the gateway has to be exposed.

- **`NodePort`**, as the manifest ships, on port `30443`. Stable across pod
  replacement, and it holds the long-lived HTTP/2 streams that watch-backed
  tables need. Dial a node address Postgres can route to.
- **A `LoadBalancer`, or an ingress with gRPC passthrough**, for production.
  Whatever sits in front must not terminate TLS, because the extension
  verifies the gateway's own certificate. A proxy that re-encrypts needs its
  certificate in Postgres's `ca_cert` instead.
- **Not `kubectl port-forward`**, except for a one-off. It drops long-lived
  streams, which is exactly what a watch-backed table depends on.

Whichever you choose, the address goes in the `endpoint` option when you
[create the server](../initialize.md#2-point-it-at-the-gateway), and must be one
of the names in the certificate. A name missing from the certificate is a
handshake failure, not a warning.

## How it runs

- **No kubeconfig.** The Deployment runs on the projected ServiceAccount token,
  which kubelet rotates. If you find yourself mounting a kubeconfig into the
  gateway, that replaces a rotating, audience-bound token with a static
  credential.
- **`strategy: Recreate`.** With one replica, a rolling update briefly runs two
  gateways, and requests can reach the one shutting down. A short gap during a
  restart is the better trade: queries see a connection error, and
  watch-backed tables keep serving cached rows with a `WARNING`.
- **A TCP readiness probe.** Kubernetes' built-in gRPC probe dials plaintext,
  and this server is TLS-only, so it would never pass. The gateway loads its
  TLS material and builds its Kubernetes client before it listens, so an open
  port does mean it got that far.

## Operating it

**Restarting** is safe at any time, and needed after an RBAC grant is revoked
or narrowed: what the gateway is allowed is cached for the life of the process.
A new grant needs no restart.

```sh
kubectl -n axiom-system rollout restart deploy/axiom-gateway
```

Watch-backed tables go `DEGRADED` across a restart, keep serving cached rows
with a `WARNING`, and resume from their bookmark rather than relisting.

**Upgrading**: move the gateway first. A newer gateway serves an older
extension, because protocol changes are additive.

**Logs** record each call, with the filters a `List` received:

```sh
kubectl -n axiom-system logs deploy/axiom-gateway --tail=50
```
