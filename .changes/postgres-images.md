---
kind: added
---

Postgres images with Axiom already installed are published to
`ghcr.io/dhilipkumars/axiom-postgres`, one per supported major:
`0.1.0-pg16`, `0.1.0-pg17`, `0.1.0-pg18`, with `latest-pgNN` tracking each
major and `latest` following the newest. Trying Axiom no longer means building
the extension.

The images set `shared_preload_libraries = 'axiom'` themselves, so a plain
`docker run` gives a Postgres where `CREATE EXTENSION axiom` works and the
background worker is already running. Passing your own
`-c shared_preload_libraries=...` still overrides it.

They are amd64 only for now, and they are for evaluating Axiom: installing it
into a Postgres you already run needs downloadable artifacts, which a later
release adds.
