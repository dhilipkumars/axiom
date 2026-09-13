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

FROM postgres:${PG_MAJOR}-bookworm
ARG PG_MAJOR
COPY --from=build /out/usr/lib/postgresql/${PG_MAJOR}/lib/axiom.so /usr/lib/postgresql/${PG_MAJOR}/lib/
COPY --from=build /out/usr/share/postgresql/${PG_MAJOR}/extension/axiom* /usr/share/postgresql/${PG_MAJOR}/extension/
