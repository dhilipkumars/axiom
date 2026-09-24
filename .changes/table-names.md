---
kind: changed
---

**Breaking: imported tables are named for their API group.**
`IMPORT FOREIGN SCHEMA` now names every table `<group>_<plural>`, with the core
group spelled `core`: `k8s.pods` is now `k8s.core_pods`, `k8s.deployments` is
`k8s.apps_deployments`, and a CloudNativePG cluster table is
`k8s.postgresql_cnpg_io_clusters`.

Previously a name depended on what else the cluster served. Installing
metrics-server, which also serves `pods` and `nodes`, renamed `k8s.pods` to
`k8s.pods_core` on the next import and broke every query and view that used
it. Two CRDs sharing a plural renamed each other the same way. A name now
depends only on its own group and resource.

**`LIMIT TO` and `EXCEPT` take the new table names**: `LIMIT TO (core_pods)`.
The old spelling imports nothing and raises a `WARNING` naming the table you
probably meant.

**To keep existing queries working**, run this once after re-importing:

```sql
SELECT * FROM axiom_create_short_names('k8s');
```

It creates views such as `k8s.pods` over the new tables. The core group gets
the bare plural. Any other plural shared by two groups is reported rather than
guessed, and `axiom_create_short_name` lets you choose. An existing object is
never replaced.
