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
background worker is already running. That is not only a convenience: Axiom
has to be preloaded, and without it `CREATE EXTENSION` fails outright rather
than running with the cache disabled. Passing your own
`-c shared_preload_libraries=...` still overrides the setting.

A `development-pgNN` tag is also published, built nightly from main rather
than on every merge, so it means "main, as of last night". Only a published
release writes a version tag or moves `latest`.

They are amd64 only for now, and they are for evaluating Axiom: installing it
into a Postgres you already run needs downloadable artifacts, which a later
release adds.
