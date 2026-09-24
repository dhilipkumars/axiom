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

**To keep existing queries working**, rebuild the schema and ask for short
names. Axiom has no extension upgrade scripts yet (#65), so a new version is a
fresh `CREATE EXTENSION` anyway. The old tables have to go first: a re-import
beside them would leave the old `k8s.pods` table in the way of the new
`k8s.pods` view.

```sql
DROP EXTENSION axiom CASCADE;   -- also drops servers, mappings, foreign tables
CREATE EXTENSION axiom;
-- recreate the server and user mapping as before, then:
CREATE SCHEMA IF NOT EXISTS k8s;
IMPORT FOREIGN SCHEMA k8s FROM SERVER prod INTO k8s;
SELECT * FROM axiom_create_short_names('k8s');
```

This creates views such as `k8s.pods` over the new tables. The core group gets
the bare plural. A plural shared by two other groups is reported rather than
guessed, and you can pick one with `axiom_create_short_name`. An existing
object is never replaced. Any views of your own that the `CASCADE` dropped can
be recreated verbatim, since `k8s.pods` exists again. Grant `SELECT` on the
short name *and* on the table behind it: the views are `security_invoker`, so
they check the querying role's privileges on both.
