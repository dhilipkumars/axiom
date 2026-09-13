---
kind: changed
---

The gateway now runs as a Kubernetes Deployment, with manifests in
`deploy/k8s/`. It authenticates with its ServiceAccount's projected token
rather than a kubeconfig, and Postgres reaches it through a `NodePort`. Running
it as a host process against a kubeconfig is still supported for development;
see the deployment guide.
