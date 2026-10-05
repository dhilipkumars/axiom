# Compatibility

What each release supports, and what CI actually runs. "Expected to work"
means the requirement is met but no test runs it; report anything that fails
there as a bug.

## Postgres

| Version | Status |
|---|---|
| 16, 17, 18 | **Supported.** Built, packaged and tested on every change. |
| 19 | Tested nightly against the current beta, as the next major. Not released. |
| 15 and older | Not supported. |

Managed Postgres (RDS, Cloud SQL, Aurora, Azure) cannot run Axiom: it needs
`shared_preload_libraries` and is not a trusted extension.

## Operating systems

The extension needs **glibc 2.34 or newer** and Linux. The `.deb` and `.rpm`
check this and refuse to install below it.

| Distribution | `.deb` | `.rpm` | Tarball |
|---|---|---|---|
| Debian 13 | **Tested** | | expected to work |
| Debian 12 | expected to work | | **Tested** |
| Ubuntu 22.04, 24.04 | expected to work | | expected to work |
| Rocky Linux 9 | | **Tested** (needs PGDG) | expected to work |
| RHEL, AlmaLinux 9 | | expected to work (needs PGDG) | expected to work |
| Debian 11, Ubuntu 20.04, RHEL 8 | refused (glibc too old) | refused | will not load |
| macOS, Windows | | | not built; [from source](guides/install/from-source.md) only, untested |

The `.rpm` targets PGDG's packaging of Postgres (`postgresql<N>-server`), not
the distribution's own module.

## Architectures

| Architecture | Status |
|---|---|
| amd64 | **Tested**: images, packages and tarballs, end to end. |
| arm64 | Built natively for every image, package and tarball. Not yet run in CI. |

Every image is multi-architecture, so Docker picks the right one and no
`--platform` flag is needed.

## Kubernetes

The gateway uses only stable APIs (discovery, OpenAPI v3, access reviews, and
list and watch), and CI runs it against the Kubernetes version current `kind`
ships. Any supported Kubernetes release is expected to work.

## Gateway and extension versions

Upgrade the gateway first. A newer gateway serves an older extension, because
protocol changes are additive. Features that need both halves, such as typed
columns, say so in the [changelog](https://github.com/dhilipkumars/axiom/blob/main/CHANGELOG.md).
