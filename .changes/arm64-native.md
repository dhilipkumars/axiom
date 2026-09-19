---
kind: fixed
---

**The guides no longer tell you to force `--platform linux/amd64`.** The
published images have been multi-architecture since v0.1.1 — every Postgres
image per major, the gateway, and the floating `latest` and `latest-pgNN` tags
— but that went unannounced, and the guides still carried a flag written when
the images were amd64-only.

On arm64 that flag was not merely unnecessary. It pinned you to the amd64
slice, so Docker emulated a machine you were already running natively.

If you copied an earlier version of the commands, drop it:

```sh
docker run -d --name axiom-postgres \
  --network kind -e POSTGRES_PASSWORD=axiom \
  -p 55432:5432 ghcr.io/dhilipkumars/axiom-postgres:latest-pg17
```

`no matching manifest for linux/arm64/v8` now means one of two things: you
pinned a version before `0.1.1`, which really is amd64-only, or the tag you
asked for was published wrongly. It is no longer a reason to add a flag.
