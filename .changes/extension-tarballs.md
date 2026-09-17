---
kind: added
---

Every release now publishes the extension as a downloadable tarball, one per
supported Postgres major and architecture — `linux/amd64` and `linux/arm64`.
Installing Axiom into a Postgres you already run no longer needs a Rust
toolchain or a checkout:

```sh
V=0.1.0; PG=17; ARCH=$(uname -m | sed -e s/x86_64/amd64/ -e s/aarch64/arm64/)
BASE=https://github.com/dhilipkumars/axiom/releases/download/v$V
curl -fsSLO "$BASE/axiom-$V-pg$PG-linux-$ARCH.tar.gz"
tar -xzf "axiom-$V-pg$PG-linux-$ARCH.tar.gz"
sudo cp -r axiom-$V-pg$PG-linux-$ARCH/usr/. /usr/
```

Each tarball carries a `.sha256` beside it and an `INSTALL.md`, and the files
come from the same build the published image is made from, so a tarball and an
image of one version contain byte-identical binaries.

They are built on Debian bookworm (glibc 2.36) and will not load on an older
glibc such as bullseye or RHEL 8 — the failure is at load time, so Postgres
refuses to start rather than half-working. Managed Postgres still cannot use
them: Axiom is not a trusted extension and needs `shared_preload_libraries`.
