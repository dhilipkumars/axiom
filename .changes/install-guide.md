---
kind: changed
---

Getting started no longer asks you to build anything. It runs a published
Postgres image, applies the gateway manifests straight from a URL, and reaches
a real `SELECT` against cluster data without cloning the repository or
installing a Rust toolchain.

The cluster step also got simpler: Postgres joins the `kind` Docker network and
dials the gateway's NodePort by the node's container name, so there is no port
mapping to add and no cluster to recreate.

Building from source is still how you install Axiom into a Postgres you
already run, and the guide says so, along with the fact that managed Postgres
(RDS, Cloud SQL, Aurora) cannot run Axiom at all — it is not a trusted
extension and needs `shared_preload_libraries`.
