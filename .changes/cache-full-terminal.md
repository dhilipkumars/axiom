---
kind: fixed
---

A subscription whose cache is full now says so. Previously an exhausted
`axiom.cache_size_mb` looked like any other write failure, and the retry
cleared the cache before relisting, discarding the rows still being served as
stale and then refilling until it hit the same limit. The subscription never
settled and the answer to a query changed each cycle.

It now reports the exhausted setting in `axiom_watch_status()` and in the
warning every stale scan raises, keeps the rows it holds, and waits at the
longest retry interval instead of relisting fruitlessly. It still recovers on
its own once anything frees space.
