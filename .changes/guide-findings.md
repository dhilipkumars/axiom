---
kind: fixed
---

Applying `deploy/k8s/gateway-deployment.yaml` now installs the newest released
gateway rather than a nightly build from `main`. Following the published guide
previously paired a released Postgres image with a development gateway — a
combination no release describes.

The agent setup guide also stops instead of adopting an existing kind cluster
called `axiom`. It would have replaced that cluster's gateway TLS secret and
restarted its gateway, and the remaining steps would then have passed while
having broken something else. It now says where to run from, so a TLS private
key does not land in whatever directory you happened to be in, checks that the
gateway's NodePort is the one the next step dials, and says the import produces
exactly two tables and which grant decides that.

**The ClusterRole and ClusterRoleBinding are renamed** from
`axiom-gateway-read` to `axiom-gateway`, matching the ServiceAccount. The name
claimed read-only and never was: ConfigMaps and the example CRD are writable,
because `INSERT`, `UPDATE` and `DELETE` on a foreign table are real Kubernetes
writes. If you applied the previous manifest, the old objects are left behind
and keep granting the same access to the same ServiceAccount — remove them once
the new ones are in place:

```sh
kubectl delete clusterrolebinding axiom-gateway-read
kubectl delete clusterrole axiom-gateway-read
```

**Check it for your own grants first.** If you added kinds by patching
`axiom-gateway-read` — which is what the previous guide told you to do —
those rules are only in that object. Copy them into `axiom-gateway` before
deleting it, or the gateway silently goes back to offering pods, configmaps
and the example CRD:

```sh
kubectl get clusterrole axiom-gateway-read -o yaml
```
