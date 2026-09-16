---
kind: added
---

A release now proves its own images can be installed before `latest` points at
them. The check pulls each published image with no credentials, asserts it
preloads Axiom and reports the version the tag claims, then runs the whole
published procedure — cluster, gateway, `IMPORT FOREIGN SCHEMA`, query — and
requires the SQL answer to match `kubectl` exactly.

It runs between publishing the version tags and moving the floating ones, so an
image that cannot be pulled or does not install stops there rather than
becoming what everyone gets by default.
