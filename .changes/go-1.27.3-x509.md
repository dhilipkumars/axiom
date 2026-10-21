---
kind: security
---

**Gateway: Go 1.27.3**, fixing GO-2026-6809: `crypto/x509` now distrusts the
compromised emSign intermediates, the same incident as RUSTSEC-2026-0381 in the
extension. The gateway verifies the Kubernetes API server's certificate. In
cluster that chains to the cluster's own CA, which those intermediates cannot
sign for. A kubeconfig that trusts public roots for its API server was exposed.
