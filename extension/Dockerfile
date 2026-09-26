# Build context: repository root (the crate's build.rs reads ../proto).
# Produces a stock postgres:16 image with the axiom extension installed.
ARG PG_MAJOR=16

# Rust 1.96 or newer: cargo-pgrx 0.19 requires it.
FROM rust:1.97-bookworm AS build
ARG PG_MAJOR
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates curl gnupg clang libclang-dev pkg-config \
        libssl-dev libreadline-dev zlib1g-dev bison flex \
    && install -d /usr/share/postgresql-common/pgdg \
    && curl -fsSL -o /usr/share/postgresql-common/pgdg/apt.postgresql.org.asc https://www.postgresql.org/media/keys/ACCC4CF8.asc \
    && echo "deb [signed-by=/usr/share/postgresql-common/pgdg/apt.postgresql.org.asc] https://apt.postgresql.org/pub/repos/apt bookworm-pgdg main" > /etc/apt/sources.list.d/pgdg.list \
    && apt-get update && apt-get install -y --no-install-recommends \
        postgresql-${PG_MAJOR} postgresql-server-dev-${PG_MAJOR} \
    && rm -rf /var/lib/apt/lists/*
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo install cargo-pgrx --version 0.19.2 --locked \
    && cargo pgrx init --pg${PG_MAJOR} /usr/lib/postgresql/${PG_MAJOR}/bin/pg_config
WORKDIR /src
COPY proto/ proto/
COPY extension/ extension/
WORKDIR /src/extension
# The staging directory is removed first, not just written over. `cargo pgrx
# package` adds files to it and never prunes, and target/ is a cache mount that
# survives a version bump -- so after one, the tree still holds the previous
# releases' axiom--<version>.sql alongside the new one. CI never sees this
# because each job starts with a cold cache; a maintainer building locally
# always does.
#
# Shipping those is wrong rather than untidy: `CREATE EXTENSION axiom VERSION
# '0.1.0'` would run an old release's SQL against this release's library and
# succeed. Only the staging directory is deleted, so nothing is recompiled --
# the object files live elsewhere under target/.
#
# COVER=1 builds the e2e suite's coverage-recording extension (#90): every
# crate compiled with -C instrument-coverage, so each backend and the worker
# write an LLVM profile when they exit, and llvm-tools matching this rustc so
# the same stage can turn those profiles into a report. Releases never set it.
# LLVM_PROFILE_FILE=/dev/null keeps the instrumented build scripts and schema
# generator that run during the build from leaving profiles in the image.
ARG COVER=""
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/extension/target \
    if [ -n "$COVER" ]; then \
        rustup component add llvm-tools \
        && export RUSTFLAGS="-C instrument-coverage" LLVM_PROFILE_FILE=/dev/null; \
    fi \
    && rm -rf target/release/axiom-pg${PG_MAJOR} \
    && cargo pgrx package --pg-config /usr/lib/postgresql/${PG_MAJOR}/bin/pg_config \
        --no-default-features --features pg${PG_MAJOR} \
    && mkdir -p /out \
    && cp -r target/release/axiom-pg${PG_MAJOR}/usr /out/

# The installable tree on its own, for `--output type=local`. Extracting it
# costs nothing extra: it exports the same build stage the image below copies
# from, so a tarball and an image of one version come from the same source by
# the same recipe. Not the same compile -- in CI they are separate jobs on
# separate runners -- so do not describe the binaries as byte-identical.
#
# Deliberately *before* the image stage. `docker build` defaults to the last
# stage, so appending this at the end would silently turn a plain
# `docker build -f extension/Dockerfile` into an artifact export and break
# every caller that expects an image.
FROM scratch AS artifact
COPY --from=build /out/usr /usr

FROM postgres:${PG_MAJOR}-bookworm
ARG PG_MAJOR
COPY --from=build /out/usr/lib/postgresql/${PG_MAJOR}/lib/axiom.so /usr/lib/postgresql/${PG_MAJOR}/lib/
COPY --from=build /out/usr/share/postgresql/${PG_MAJOR}/extension/axiom* /usr/share/postgresql/${PG_MAJOR}/extension/

# Axiom cannot be loaded any other way. `_PG_init` defines two PGC_POSTMASTER
# GUCs, and Postgres refuses to create those after startup, so loading the
# library on demand does not degrade: `CREATE EXTENSION axiom` fails, naming
# this setting and saying to restart. Setting it here means a plain
# `docker run` of this image never meets that.
#
# The sample is what initdb templates postgresql.conf from, so putting it here
# means a plain `docker run` of this image works without the person running it
# having to know any of the above. It does not take away the choice: a
# `-c shared_preload_libraries=...` on the command line still wins, which is
# how the compose stack and the E2E gates set their own value.
#
# The majored path is a symlink to /usr/share/postgresql/postgresql.conf.sample
# in the current base image. Appending through it writes to the real file
# either way, which keeps this correct if that stops being a symlink.
RUN printf "\n# Axiom's background worker and shared cache require preloading.\nshared_preload_libraries = 'axiom'\n" \
      >> /usr/share/postgresql/${PG_MAJOR}/postgresql.conf.sample
