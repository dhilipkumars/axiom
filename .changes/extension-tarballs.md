---
kind: added
---

Releases from this one onward publish the extension as a downloadable
tarball, one per supported Postgres major and architecture — `linux/amd64`
and `linux/arm64`. Installing Axiom into a Postgres you already run no longer
needs a Rust toolchain or a checkout. Set `V` to this release's version:

```sh
V=          # this release, without the leading v
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
