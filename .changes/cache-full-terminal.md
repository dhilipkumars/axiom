---
kind: fixed
---

A subscription whose cache is full now says so. Previously an exhausted
`axiom.cache_size_mb` looked like any other write failure: the subscription
reconnected once a second and walked into the same wall each time. For one that
filled while still building its cache, every attempt was a fresh listing of the
whole collection, which is real work for the gateway and the API server and
could never succeed; for one that had already synced, the retry resumed from
its bookmark and replayed the same failing event.

It now reports the exhausted setting in `axiom_watch_status()` and waits at the
longest retry interval instead of retrying fruitlessly. Scans say so too: a
cache that filled after syncing keeps being served stale with a warning on
every scan, and one that filled while still building is not served at all --
those scans fall back to the gateway, and now warn that they did, so the person
running the query learns what the person reading the logs would.

Recovery happens on its own when a sweep reclaims expired tombstones.
Otherwise raise `axiom.cache_size_mb` and restart.
