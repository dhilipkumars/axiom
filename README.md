# Axiom

A Postgres foreign data wrapper for Kubernetes: query and control built-in
resources and CRDs across clusters as foreign tables, with watch-driven live
updates. See [docs/DESIGN.md](docs/DESIGN.md) for the architecture,
[docs/PLAN.md](docs/PLAN.md) for the phased plan, and
[docs/RULES.md](docs/RULES.md) for the engineering gates every phase must meet.

## Status

**Phase 0 — skeleton & plumbing.** No Kubernetes code yet. What exists:

| Component | Path | What it does |
|---|---|---|
| Protobuf API | `proto/axiom/v1/axiom.proto` | `GatewayService.Ping` only |
| Gateway (Go) | `gateway/` | TLS-only gRPC server serving `Ping` (no plaintext mode) |
| Extension (Rust/pgrx) | `extension/` | `axiom.*` GUCs + a background worker that pings the gateway over TLS on a timer and logs the outcome |
| Local stack | `deploy/compose/` | `docker compose` brings up gateway + Postgres with a throwaway CA |
| E2E | `e2e/phase0.sh` | Starts the stack, `CREATE EXTENSION axiom`, asserts a `ping ok` round-trip |

## Prerequisites

- Rust stable, [`cargo-pgrx` 0.12.9](https://github.com/pgcentralfoundation/pgrx) initialised for a local Postgres (`cargo pgrx init --pg16 $(which pg_config)`), `cargo-audit`, `cargo-deny`
- Go 1.25+, [`buf`](https://buf.build), `golangci-lint` v2
- Docker with Compose v2

## Common tasks

```sh
make proto           # lint the proto and regenerate Go stubs (Rust stubs are built by build.rs)
make lint            # golangci-lint + cargo fmt/clippy (pedantic, -D warnings)
make unit            # go test -race + cargo pgrx test
make ext-test PG=pg16
make e2e-phase0      # full compose-based Phase 0 E2E
make up / make down  # keep the local stack around for poking at
```

The extension's Postgres tests start a real Postgres; on macOS use a short
`CARGO_TARGET_DIR` (e.g. `~/.cache/axiom-target`) so the Unix socket path
stays under the OS limit.

## Extension configuration

The background worker only starts when the library is preloaded:

```
shared_preload_libraries = 'axiom'
axiom.gateway_endpoint   = 'https://gateway:8443'   # https only, no embedded credentials
axiom.gateway_ca_cert    = '/certs/ca.crt'          # optional; default is the webpki root store
axiom.ping_interval_secs = 10                        # must exceed rpc_timeout_secs
axiom.rpc_timeout_secs   = 5
```

All four are `SIGHUP`-reloadable. Ping outcomes are logged with a stable prefix,
`axiom bgworker: ping ok ...` at `LOG` and `axiom bgworker: ping failed ...` at
`WARNING`, with exponential backoff (1s–60s) on failure.
