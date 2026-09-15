# Changelog

What changed in each release, from the point of view of someone using Axiom.
Entries are written during development as files in [`.changes/`](.changes/) and
assembled here when a release is cut, by `scripts/changelog release <version>`.

Unreleased work has no section here. To see what is pending, run
`make changelog` — it renders the changesets that will make up the next
release without consuming them.

Versions follow [semantic versioning](https://semver.org). While the major
version is `0`, the SQL surface and the gateway's gRPC API may change between
minor versions; the changelog says so when they do.

<!-- releases below -->
