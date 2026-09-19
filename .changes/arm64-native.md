---
kind: changed
---

**The published images run natively on arm64**, so nothing needs
`--platform linux/amd64` any more and nothing runs under emulation. Every
image — the Postgres images per major, and the gateway — is published for
`linux/amd64` and `linux/arm64` from v0.1.1 on, including the floating
`latest` and `latest-pgNN` tags.

The guides no longer carry the flag. If you copied an earlier version of them,
drop it: leaving it in pins you to the amd64 slice and makes Docker emulate a
machine you are already running natively.

    docker run -d --name axiom-postgres \
      --network kind -e POSTGRES_PASSWORD=axiom \
      -p 55432:5432 ghcr.io/dhilipkumars/axiom-postgres:latest-pg17

`no matching manifest for linux/arm64/v8` now means something narrower than it
used to: the version you pinned predates v0.1.1. Use `latest-pgNN`, or a
version from `0.1.1` on.
