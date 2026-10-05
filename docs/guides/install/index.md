# Install

Two things get installed: the **gateway** in your cluster, and the **extension**
in Postgres. The gateway is the same whichever way Postgres gets the extension.

1. **[The gateway](gateway.md)**, in the cluster. Always.
2. **Axiom in Postgres**, one of:

    | Route | For |
    |---|---|
    | [Postgres image](postgres-image.md) | trying Axiom, or running a new Postgres in Docker |
    | [Packages](packages.md) | a Postgres you already run, on Linux: `.deb`, `.rpm` or tarball |
    | [From source](from-source.md) | a platform no package covers |

3. **[Restrict what the gateway can see](rbac.md)**, if the shipped read access
   is broader than you want.

Then [initialize](../initialize.md) Axiom: create the extension, point it at
the gateway, and import the cluster's tables.

Just want to see it work? The [quick start](../quick-start.md) does all of this
on a local kind cluster with one script.
