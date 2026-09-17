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
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/extension/target \
    cargo pgrx package --pg-config /usr/lib/postgresql/${PG_MAJOR}/bin/pg_config \
        --no-default-features --features pg${PG_MAJOR} \
    && mkdir -p /out \
    && cp -r target/release/axiom-pg${PG_MAJOR}/usr /out/

# The installable tree on its own, for `--output type=local`. Extracting it
# costs nothing extra: it is the same build stage the image below copies from,
# so building the tarball and the image is one compile, not two.
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
