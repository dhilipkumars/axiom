# Packages

For a Postgres you already run on Linux. Each release publishes a `.deb`, an
`.rpm` and a tarball per Postgres major and architecture, on the
[releases page](https://github.com/dhilipkumars/axiom/releases). No toolchain,
no rebuild.

Prefer a package to the tarball where you can. It can be removed again, and it
**refuses to install on a system whose glibc is too old**, instead of letting
you find out when Postgres will not restart. [Compatibility](../../compatibility.md)
lists what is tested.

## 1. Install the files

=== "Debian, Ubuntu"

    ```sh
    V=0.2.0; PG=17
    BASE=https://github.com/dhilipkumars/axiom/releases/download/v$V
    ARCH=$(dpkg --print-architecture)
    curl -fsSLO "$BASE/postgresql-$PG-axiom_$V-1_$ARCH.deb"
    sudo apt install "./postgresql-$PG-axiom_$V-1_$ARCH.deb"
    ```

    Installs under `/usr/lib/postgresql/$PG`, beside the server package from
    the distribution or PGDG, without owning those directories.

=== "RHEL, Rocky, Alma 9"

    ```sh
    V=0.2.0; PG=17
    BASE=https://github.com/dhilipkumars/axiom/releases/download/v$V
    curl -fsSLO "$BASE/axiom_$PG-$V-1.el9.$(uname -m).rpm"
    sudo dnf install "./axiom_$PG-$V-1.el9.$(uname -m).rpm"
    ```

    Built for **PGDG's** Postgres (`postgresql$PG-server`, under
    `/usr/pgsql-$PG`), so enable PGDG's repository first. The distribution's
    own Postgres module is not supported.

=== "Tarball"

    ```sh
    V=0.2.0; PG=17; ARCH=$(uname -m | sed -e s/x86_64/amd64/ -e s/aarch64/arm64/)
    BASE=https://github.com/dhilipkumars/axiom/releases/download/v$V
    curl -fsSLO "$BASE/axiom-$V-pg$PG-linux-$ARCH.tar.gz"
    curl -fsSLO "$BASE/axiom-$V-pg$PG-linux-$ARCH.tar.gz.sha256"
    sha256sum -c "axiom-$V-pg$PG-linux-$ARCH.tar.gz.sha256"
    tar -xzf "axiom-$V-pg$PG-linux-$ARCH.tar.gz"
    ```

    It holds `axiom.so` and the extension's control and SQL files, under the
    paths a Debian-packaged Postgres uses:

    ```sh
    sudo cp -r axiom-$V-pg$PG-linux-$ARCH/usr/. /usr/
    ```

    For any other layout, place the two pieces where `pg_config` says:

    ```sh
    sudo cp axiom-$V-pg$PG-linux-$ARCH/usr/lib/postgresql/$PG/lib/axiom.so \
      "$(pg_config --pkglibdir)/"
    sudo cp axiom-$V-pg$PG-linux-$ARCH/usr/share/postgresql/$PG/extension/axiom* \
      "$(pg_config --sharedir)/extension/"
    ```

**Match the major exactly.** A `pg17` build is compiled against Postgres 17's
headers and does not work with any other major.

## 2. Preload it, and restart

No package can do this part for you. Axiom keeps a cache in shared memory and
runs a background worker, both of which exist only if the library loads at
startup, so `CREATE EXTENSION` fails outright without it.

```ini
# postgresql.conf -- append to any existing list rather than replacing it
shared_preload_libraries = 'axiom'
```

**Append, don't overwrite.** The setting is one comma-separated list, so write
`'pg_stat_statements,axiom'` if something is already there. Replacing it
silently disables whatever was loaded before, at the next restart.

Then restart Postgres. Two more settings are fixed at startup and worth
knowing:

| Setting | Default | What it does |
|---|---|---|
| `axiom.cache_size_mb` | `256` | Upper bound of the shared-memory cache for watch-backed tables. |
| `axiom.notify_database` | `postgres` | Database the worker sends `NOTIFY axiom_events` in. |

## 3. Give Postgres the gateway's CA

Copy the gateway's `ca.crt` to the database host, somewhere the user Postgres
runs as can read. Its path is what the `ca_cert` option takes. Skip this if the
gateway's certificate is signed by a CA the host already trusts.

Next: [initialize](../initialize.md).
