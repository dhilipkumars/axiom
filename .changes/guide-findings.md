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
