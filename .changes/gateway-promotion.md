---
kind: fixed
---

**`axiom-gateway:latest` now moves only after the release has been proven to
install**, in the same step as the Postgres images' `latest` and `latest-pgNN`.

Previously it moved as soon as both architectures were built, while the
Postgres tags waited for the install check. A release that failed that check
therefore left the Postgres floating tags correctly untouched and the gateway's
already advanced — so pulling both without pinning gave a new gateway against
the *previous* Postgres release, a pairing no release describes and nothing
tested.

This only ever affected unpinned pulls of a release that failed its own check.
If you pin versions, nothing changes.
