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

