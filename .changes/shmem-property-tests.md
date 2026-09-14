---
kind: added
---

`axiom_watch_status()` now reports a `tombstones` column: objects deleted in
the cluster that the cache still holds for the grace period before sweeping
them. They are never returned by a scan, but they occupy cache memory, so a
subscription whose tombstone count keeps climbing is worth knowing about.
