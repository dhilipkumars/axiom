# Changelog

What changed in each release, from the point of view of someone using Axiom.
Entries are written during development as files in [`.changes/`](.changes/) and
assembled here when a release is cut, by `scripts/changelog release <version>`.

Unreleased work has no section here. To see what is pending, run
`make changelog` — it renders the changesets that will make up the next
release without consuming them.

Versions follow [semantic versioning](https://semver.org). While the major
version is `0`, the SQL surface and the gateway's gRPC API may change between
minor versions; the changelog says so when they do. A patch release can still
carry an `### Added` entry — the changelog describes what changed, the version
describes what Axiom can now do, and new ways to install the same functionality
are the former. [docs/RELEASING.md](docs/RELEASING.md) has the full rule.

<!-- releases below -->

## 0.2.0

### Added

**Usage and events are queryable, and their numbers behave like numbers.**
`metrics.k8s.io` reaches SQL as `metrics_k8s_io_pods` and
`metrics_k8s_io_nodes` alongside the events tables, so consumption, live spec
and failure reasons can be joined in one statement — a capacity and risk
review of a whole fleet as a single query.

Kubernetes reports measurements as strings with unit suffixes, which Postgres
cannot compare or sum: `WHERE cpu > '1'` is a string comparison that answers
wrongly without erroring. The new `axiom_quantity()` converts them exactly —
`100m` is `0.1`, `128Mi` is `134217728`, and `1M` is not `1Mi` — returning
`NULL` for anything malformed so one odd field cannot fail a cluster-wide
query.

The shipped gateway RBAC now reads broadly: everything in Kubernetes' `view`
role (workloads, ConfigMaps, Services, NetworkPolicies, Ingresses, Events),
plus nodes, storage, CRDs, RBAC objects, `events.k8s.io` and the resource,
custom and external metrics APIs. **Secrets are excluded, and cannot be
reached through it.** Custom resources are included when their operator ships
an `aggregate-to-view` role, or when you label a read-only ClusterRole with
`axiom.dhilipkumars.github.io/aggregate-to-gateway: "true"`. Mutating verbs
stay enumerated per resource.

A new RBAC grant, such as a labelled ClusterRole for a CRD, now appears on the
next import without restarting the gateway. Revoking a grant still needs a
restart.

Releases now publish **`.deb` and `.rpm` packages** beside the tarballs, one
per Postgres major and architecture. Installing into a Postgres you already
run is a package manager command rather than a `cp`:

```sh
# Debian / Ubuntu
sudo apt install ./postgresql-17-axiom_<version>-1_amd64.deb

# RHEL / Rocky / Alma 9, with PGDG's repo enabled
sudo dnf install ./axiom_17-<version>-1.el9.x86_64.rpm
```

Each follows its own convention, so the package looks native on either side:
Debian's `postgresql-<major>-axiom` installing under
`/usr/lib/postgresql/<major>`, and PGDG's `axiom_<major>` under
`/usr/pgsql-<major>`. They own the three extension files and not the
directories, so neither conflicts with the Postgres server package. `apt
remove` and `dnf remove` take Axiom away again, which a tarball never offered.

**The reason to prefer a package is what happens when you are on the wrong
distro.** Both declare the glibc floor the binary actually has, so the package
manager refuses up front and installs nothing:

    nothing provides libc.so.6(GLIBC_2.29)(64bit) needed by axiom_17

Previously that machine accepted every file, and you found out after editing
`shared_preload_libraries` and restarting -- at which point Postgres would not
start and the cause looked like Axiom rather than like a download for the wrong
system.

**The supported floor is glibc 2.34**, which is Debian 12+, Ubuntu 22.04+ and
RHEL 9+. Earlier notes derived this from the build image's glibc and said 2.36,
which wrongly excluded Ubuntu 22.04 and RHEL 9 -- both run Axiom. Debian
bullseye and RHEL 8 are genuinely too old. The floor is now read from the
binary at package time rather than written down, so it cannot drift again.

Tarballs are unchanged and remain the option where no package manager applies.

**Columns read as the type you declare them.** A foreign table column can now
be `bigint`, `boolean` or `timestamptz` as well as `text` and `jsonb`, and then
compares and sorts as that type, with no cast. `ALTER FOREIGN TABLE
k8s.core_events ALTER COLUMN count TYPE bigint` makes `ORDER BY count DESC`
numeric, and `creation_timestamp` can be `timestamptz`, so
`WHERE creation_timestamp < now() - interval '7 days'` works as written.

A value that is not of the declared type reads as NULL. Writes produce JSON of
the same type: `SET count = 5` writes the number `5`, not the string `"5"`.

A table you write by hand keeps the types you declare: a column declared
`text` reads as it always did.

### Changed

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

**`IMPORT FOREIGN SCHEMA` gives columns the type the kind's schema declares.**
A field Kubernetes declares as a string, integer, boolean or timestamp is now
imported as `text`, `bigint`, `boolean` or `timestamptz`, so the query you
would naturally write works:

```sql
SELECT namespace, name, reason, count FROM k8s.core_events
 WHERE type = 'Warning' ORDER BY count DESC;
```

`creation_timestamp` is `timestamptz` and a Deployment's replica counts are
`bigint`, so `ORDER BY replicas DESC` puts 10 above 9. Objects, arrays and
fields that may hold more than one type, such as quantities, stay `jsonb`.

**Re-importing changes column types**, and upgrading from 0.1.x re-imports
(see *Upgrading* below). A query written for the old types
fails with an error rather than answering differently: `type->>0` on what is
now a `text` column is `operator does not exist`; write `type = 'Warning'`.
Casts such as `replicas::int` still work.

The gateway and extension must both be this version or later for typed
imports; either one alone keeps importing `text` and `jsonb` as before.

### Fixed

**The guides no longer tell you to force `--platform linux/amd64`.** The
published images have been multi-architecture since v0.1.1 — every Postgres
image per major, the gateway, and the floating `latest` and `latest-pgNN` tags
— but that went unannounced, and the guides still carried a flag written when
the images were amd64-only.

On arm64 that flag was not merely unnecessary. It pinned you to the amd64
slice, so Docker emulated a machine you were already running natively.

If you copied an earlier version of the commands, drop it:

```sh
docker run -d --name axiom-postgres \
  --network kind -e POSTGRES_PASSWORD=axiom \
  -p 55432:5432 ghcr.io/dhilipkumars/axiom-postgres:latest-pg17
```

`no matching manifest for linux/arm64/v8` now means one of two things: you
pinned a version before `0.1.1`, which really is amd64-only, or the tag you
asked for was published wrongly. It is no longer a reason to add a flag.

**An INSERT writes a `camelCase` field under its real name.** Setting a column
such as `string_data` on an imported table wrote `string_data` into the new
object instead of `stringData`, and the API server dropped it without an error:
the Secret was created empty. It affected every top-level field Kubernetes
spells in `camelCase`, including `stringData`, `binaryData` and a CRD's own.

`IMPORT FOREIGN SCHEMA` now records each such field's spelling on its column,
as `string_data jsonb OPTIONS (field 'stringData')`. A column's `OPTIONS` are
now validated too, so a misspelt option is an error rather than ignored.

Upgrading from 0.1.x re-imports every table, which sets the option. Tables you
imported with a 0.2.0 release candidate still write the old name: import them
again, or add the option to the columns you write:

```sql
ALTER FOREIGN TABLE k8s.core_secrets
  ALTER COLUMN string_data OPTIONS (ADD field 'stringData');
```

Hand-written tables without the option behave as before. Any gateway version
works: it already sent each field's spelling.

**`axiom-gateway:latest` now moves only after the release has been proven to
install**, in the same step as the Postgres images' `latest` and `latest-pgNN`.

Previously it moved as soon as both architectures were built, while the
Postgres tags waited for the install check. A release that failed that check
therefore left the Postgres floating tags correctly untouched and the gateway's
already advanced — so pulling both without pinning gave a new gateway against
the *previous* Postgres release, a pairing no release describes and nothing
tested.

This only ever affected unpinned pulls of a release that failed its own check.
If you pin versions, nothing changes.

**`INSERT` no longer discards `raw`.** Inserting a whole manifest as `raw`
created an empty object and reported success. Postgres fills every column an
`INSERT` does not mention with NULL, and each NULL cleared the matching field,
so `data`, `labels` and `annotations` from `raw` were wiped. A NULL column now
leaves `raw` alone.

When both are given, a typed column overrides the same field in `raw`, as it
already does on `UPDATE`. That makes `raw` read from one object usable as a
template for another: server-assigned metadata such as `uid` and
`creationTimestamp` is dropped rather than sent. A `raw` whose `apiVersion` or
`kind` names a different kind from the table is now refused. Before, it was
silently relabelled, on `INSERT` and on an `UPDATE` that replaces `raw`.

**Listing no longer fails when a later page is larger than the first.** On a
cluster whose objects vary widely in size -- CustomResourceDefinitions that
embed large schemas beside small ones, or ConfigMaps of very different sizes
-- a query could fail with `ResourceExhausted: ... cannot be split further`
even though every object fit. Only the first page of a listing could be made
smaller to fit a response; a later one was stuck with the first page's size.

A later page is now fetched again at a smaller size when it is too large, for
kinds served by Kubernetes itself (built-in kinds and CRDs), and every object
still arrives exactly once. This applies to watch-mode tables as well.
Aggregated APIs such as metrics-server keep the previous behaviour. A single
object too large for one response is still reported as before.

The gateway decides "served by Kubernetes itself" from the `apiservices`
object for the group, which the shipped RBAC can read. A deployment whose
RBAC cannot read it keeps the previous behaviour.

### Security

**`axiom_watch_status()` is no longer callable by every role.** It lists every
watched server, resource and namespace, with object counts and resource
versions, whatever the caller's table grants. So a role granted only a narrow
view could learn what the cluster is being watched for. Only superusers can
call it now. Grant it to the roles that monitor Axiom:

```sql
GRANT EXECUTE ON FUNCTION axiom_watch_status() TO monitoring;
```

A new guide, **Giving an AI agent access**, sets out a role that gives an
agent redacted, tenant-scoped access to the cluster. The agent gets no shell
and no Kubernetes credential. The guide also covers the grants never to give
an agent and the settings that look like controls but are not. An end-to-end
test runs its recipe against a real cluster.

### Upgrading from 0.1.x

Axiom keeps no data of its own, so upgrading is re-running setup:

1. Move the gateway to `v0.2.0` and re-apply `deploy/k8s/gateway-rbac.yaml`,
   which grants the new kinds. A 0.2.0 gateway serves a 0.1.x extension, so
   this can go first.
2. Install the 0.2.0 packages, then `DROP EXTENSION axiom CASCADE` and run
   `CREATE EXTENSION`, `CREATE SERVER` and `IMPORT FOREIGN SCHEMA` again. That
   picks up the new table names and column types.

`CASCADE` takes views built on Axiom's tables with it, so recreate those from
your scripts. `ALTER EXTENSION axiom UPDATE` is planned for a coming release
(#65).

## 0.1.1

### Added

Releases from this one onward publish the extension as a downloadable
tarball, one per supported Postgres major and architecture — `linux/amd64`
and `linux/arm64`. Installing Axiom into a Postgres you already run no longer
needs a Rust toolchain or a checkout.

```sh
V=0.1.1
PG=17; ARCH=$(uname -m | sed -e s/x86_64/amd64/ -e s/aarch64/arm64/)
BASE=https://github.com/dhilipkumars/axiom/releases/download/v$V
curl -fsSLO "$BASE/axiom-$V-pg$PG-linux-$ARCH.tar.gz"
curl -fsSLO "$BASE/axiom-$V-pg$PG-linux-$ARCH.tar.gz.sha256"
sha256sum -c "axiom-$V-pg$PG-linux-$ARCH.tar.gz.sha256"
tar -xzf "axiom-$V-pg$PG-linux-$ARCH.tar.gz"
sudo cp -r axiom-$V-pg$PG-linux-$ARCH/usr/. /usr/
```

(v0.1.0 predates this and has no tarballs.)

Each tarball carries a `.sha256` beside it and an `INSTALL.md`, and the files
are exported from the same Dockerfile stage the published image is built from,
so a tarball and an image of one version are made from the same source by the
same recipe.

They are built on Debian bookworm (glibc 2.36) and will not load on an older
glibc such as bullseye or RHEL 8 — the failure is at load time, so Postgres
refuses to start rather than half-working. Managed Postgres still cannot use
them: Axiom is not a trusted extension and needs `shared_preload_libraries`.

### Fixed

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

The first query after about 40 seconds of an idle session no longer fails with
`cannot reach gateway ... transport error`. The gateway pings a peer that has
been idle for 30 seconds and drops it 10 seconds later, and a Postgres backend
sitting between queries has nothing running to answer with — so the gateway
closed a connection the backend still had cached, and the next statement paid
for discovering it.

A cached connection is now rebuilt once it has been unused for 25 seconds,
before the gateway has even started asking. Reads and writes alike: previously
a transaction that paused and then wrote lost the write and aborted, with no
way to retry, and a pooled client hit this on every gap longer than the
keepalive window.


## 0.1.0

The first release. Axiom lets you query and control Kubernetes resources —
built-in kinds and CRDs alike — from plain SQL, from a Postgres that may sit
entirely outside the cluster's network. `SELECT` reads the cluster,
`INSERT`/`UPDATE`/`DELETE` map to real Kubernetes writes with
optimistic-concurrency conflicts surfaced as SQL errors, and a table declared
`cache_mode 'watch'` is served from a shared-memory cache kept current by a
standing watch.

**What you get.** Postgres images with the extension already installed, one per
supported major — `ghcr.io/dhilipkumars/axiom-postgres:0.1.0-pg16`, `-pg17`,
`-pg18` — and the gateway image the cluster side runs. Start at
[Getting started](https://dhilipkumars.github.io/axiom/guides/getting-started/),
or point a coding agent at
[llms.txt](https://dhilipkumars.github.io/axiom/llms.txt).

**This is a release to evaluate, not to adopt.** Read these before you plan
around it:

- **No downloadable extension artifacts yet.** Installing into a Postgres you
  already run means building from source; the images are the supported path.
- **Managed Postgres cannot run Axiom at all.** It is not a trusted extension
  and needs `shared_preload_libraries`, so RDS, Cloud SQL and Aurora are out.
- **amd64 only.** On arm64 the images run under emulation, with
  `--platform linux/amd64`.
- **One identity for everyone.** What a `SELECT` can reach is decided by the
  gateway's RBAC, not the caller's, so grant the gateway only what every user
  of that database should be able to read.
- **One cluster per server.** Querying several means several servers.
- **Pre-1.0.** The SQL surface and the gRPC API may change between minor
  versions; this changelog will say when they do.

The rest of this section is the development history that produced it, kept
because it records why things are the way they are. On a first release there is
no previous version to compare against, so read "now" as "as shipped".

### Added

A setup guide written for coding agents, at
[guides/for-agents](https://dhilipkumars.github.io/axiom/guides/for-agents/):
the same path as getting started, but with a verification and expected output
for every step, failure signatures mapped to actions, idempotent commands, and
a single success criterion. Point an agent at it and it can bring up a working
environment without guessing.

The site also serves an [llms.txt](https://dhilipkumars.github.io/axiom/llms.txt)
index, following the emerging convention, so a model given the site root can
find the right page and the constraints that matter — RBAC bounds what a query
can reach, managed Postgres cannot run Axiom, images are amd64 only.

Documentation is published at https://dhilipkumars.github.io/axiom/. The
reference pages for the gRPC API, the gateway's flags, the FDW options and the
foreign-table columns are generated from the code by `make docs-generate`, and
CI fails if they are out of date.

`axiom_gateway_stats('<server>')` reports a gateway's per-RPC call counters, so
a cache-served scan can be told from an on-demand one without reading the
gateway's log. The counters are per-process and reset when the gateway
restarts.

A release now proves its own images can be installed before `latest` points at
them. The check pulls each published image with no credentials, asserts it
preloads Axiom and reports the version the tag claims, then runs the whole
published procedure — cluster, gateway, `IMPORT FOREIGN SCHEMA`, query — and
requires the SQL answer to match `kubectl` exactly.

It runs between publishing the version tags and moving the floating ones, so an
image that cannot be pulled or does not install stops there rather than
becoming what everyone gets by default.

Postgres 18 is supported. Two upstream changes needed handling: the planner's
`create_foreignscan_path` gained a `disabled_nodes` argument, and tuple
descriptors replaced their inline attribute array with compact attributes, so
reading a column's name and type goes through a version-gated accessor.

Postgres images with Axiom already installed are published to
`ghcr.io/dhilipkumars/axiom-postgres`, one per supported major:
`0.1.0-pg16`, `0.1.0-pg17`, `0.1.0-pg18`, with `latest-pgNN` tracking each
major and `latest` following the newest. Trying Axiom no longer means building
the extension.

The images set `shared_preload_libraries = 'axiom'` themselves, so a plain
`docker run` gives a Postgres where `CREATE EXTENSION axiom` works and the
background worker is already running. That is not only a convenience: Axiom
has to be preloaded, and without it `CREATE EXTENSION` fails outright rather
than running with the cache disabled. Passing your own
`-c shared_preload_libraries=...` still overrides the setting.

A `development-pgNN` tag is also published, built nightly from main rather
than on every merge, so it means "main, as of last night". Only a published
release writes a version tag or moves `latest`.

They are amd64 only for now, and they are for evaluating Axiom: installing it
into a Postgres you already run needs downloadable artifacts, which a later
release adds.

`axiom_watch_status()` now reports a `tombstones` column: objects deleted in
the cluster that the cache still holds for the grace period before sweeping
them. They are never returned by a scan, but they occupy cache memory, so a
subscription whose tombstone count keeps climbing is worth knowing about.

### Changed

**RBAC is now the source of truth for what a gateway exposes.** The guides
described two bounds — an allowlist and the ServiceAccount's RBAC — and told
you to keep them in step. Only one of them is enforced by the API server, so
the other was a way to be confused rather than a way to be safe. The guides
now teach RBAC alone: to change what appears in SQL, change the ClusterRole.

The `--serve` allowlist still exists and still works, as narrowing for a
gateway that should offer less than its ServiceAccount permits, but it is no
longer part of how Axiom is explained and is expected to be deprecated.

The deployment manifest therefore no longer sources `AXIOM_SERVE` from an
`axiom-gateway-config` ConfigMap; it is a literal that narrows nothing, and
nothing reads that ConfigMap any more.

Two operational claims were also wrong and are corrected. A kind removed from
the cluster does **not** stay on offer until the gateway restarts — resource
lists expire after `-discovery-ttl`, five minutes by default. Restarting is
required after an RBAC change, whose access decisions really are cached for
the process lifetime.

The README now links the documentation site from the top, and the guides
describe unshipped work as "on the roadmap" rather than by phase number, which
meant nothing to a reader who is not working on Axiom.

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

**Breaking: Postgres 14 and 15 are no longer supported.** The supported window
is now 16 through the latest major, currently 16, 17 and 18, and every one of
them is built and tested in CI.

14 reaches end of life in November 2026 and 15 is not where the installed base
sits; 16 is supported upstream until November 2028. Building the extension now
requires Rust 1.96 or newer, because it moved to pgrx 0.19.

The gateway now runs as a Kubernetes Deployment, with manifests in
`deploy/k8s/`. It authenticates with its ServiceAccount's projected token
rather than a kubeconfig, and Postgres reaches it through a `NodePort`. Running
it as a host process against a kubeconfig is still supported for development;
see the deployment guide.

### Fixed

A subscription whose cache is full now says so. Previously an exhausted
`axiom.cache_size_mb` looked like any other write failure: the subscription
reconnected once a second and walked into the same wall each time. For one that
filled while still building its cache, every attempt was a fresh listing of the
whole collection, which is real work for the gateway and the API server and
could never succeed; for one that had already synced, the retry resumed from
its bookmark and replayed the same failing event.

It now reports the exhausted setting in `axiom_watch_status()` and waits at the
longest retry interval instead of retrying fruitlessly. Scans say so too: a
cache that filled after syncing keeps being served stale with a warning on
every scan, and one that filled while still building is not served at all --
those scans fall back to the gateway, and now warn that they did, so the person
running the query learns what the person reading the logs would.

Recovery happens on its own when a sweep reclaims expired tombstones.
Otherwise raise `axiom.cache_size_mb` and restart.

A scan of a large collection no longer fails outright. `List` is now paged, so
a table with more objects than fit in one gRPC message is read across several
requests instead of exceeding the 4 MiB default and erroring. Both ends now set
an explicit 16 MiB message limit as a backstop, and a page is bounded by bytes
as well as by object count, because a count that suits Pods can be hundreds of
megabytes of ConfigMaps.

A watch subscription's initial listing is paged the same way, so starting a
watch on a large kind no longer pulls the whole collection into gateway memory
before the first event.

Installing Axiom without `shared_preload_libraries` now says so. It previously
failed with `FATAL: cannot create PGC_POSTMASTER variables after startup`,
which names neither Axiom nor the setting that fixes it, and which took the
client's connection down with it.

It is now an ordinary error naming the extension, the setting, and the restart:
the statement fails and the session stays open.

```
ERROR:  axiom must be loaded through shared_preload_libraries
DETAIL:  Add `shared_preload_libraries = 'axiom'` to postgresql.conf, restart
Postgres, then run CREATE EXTENSION axiom. ...
```

Nothing changes for a correctly preloaded Postgres, which includes the
published images — they preload Axiom themselves.

The guides and README described the old behaviour — a warning, and watch tables
falling back to an RPC per scan. Neither can happen now, so both say what
actually does.

Three things found by using Axiom by hand, all operator-facing:

- A kind deleted from the cluster stopped being offered only when the gateway
  was restarted. The cached resource list now expires after five minutes, so a
  re-import reflects the cluster.
- An `IMPORT FOREIGN SCHEMA` that ran out of time reported only that a deadline
  expired, without saying which setting governs it. It now names
  `rpc_timeout_secs` and suggests importing one API group at a time. The
  deadline itself is unchanged.
- Scanning a table whose kind the gateway no longer serves said only
  "unsupported kind". It now explains that the cause is the cluster, the serve
  list or RBAC, and that the table needs re-importing. It stays an error rather
  than becoming a warning with zero rows, which would be indistinguishable from
  an empty cluster, and it still names all three causes rather than the actual
  one, so the serve list cannot be enumerated by probing.

### Security

Updated rustls to 0.23.45, which fixes RUSTSEC-2026-0285: TLS 1.3 handshake
messages were accepted across encryption level boundaries. rustls terminates
the extension's side of the Postgres-to-gateway connection, so this sits on the
boundary that carries every cluster read and write.

