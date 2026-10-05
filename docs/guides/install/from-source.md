# From source

Only when no [package](packages.md) matches your platform. You need a Rust
toolchain and `cargo-pgrx` matching the `pg_config` of the Postgres you are
installing into.

```sh
git clone --branch v0.2.0 https://github.com/dhilipkumars/axiom.git
cd axiom
cargo install cargo-pgrx --version 0.19.2 --locked
cargo pgrx init --pg17 "$(which pg_config)"
cd extension && cargo pgrx install --release --no-default-features --features pg17
```

- **Three places name the major, and all must agree:** `--pg17` on
  `cargo pgrx init`, `--features pg17`, and the `pg_config` you point at.
  Change all three for another major, or the build fails without naming the
  cause.
- `cargo pgrx install` writes into the directories `pg_config` reports, so run
  it as a user who may write there, or with `sudo -E` to keep the toolchain on
  `PATH`.
- Only Linux is tested. macOS builds, but nothing in CI runs it.

Then preload it exactly as for a package:
[Preload it, and restart](packages.md#2-preload-it-and-restart).

The gateway builds with plain `go build ./cmd/gateway` under `gateway/`; the
[developer guide](../../development.md) covers both halves in depth.
