---
kind: fixed
---

A subscription whose cache is full now says so. Previously an exhausted
`axiom.cache_size_mb` looked like any other write failure: the subscription
reconnected and relisted once a second, walking into the same wall each time
and costing the gateway and the API server a full listing per attempt for no
possible progress.

It now reports the exhausted setting in `axiom_watch_status()`, and in the
warning every scan raises when the cache is still servable, and waits at the
longest retry interval instead of relisting fruitlessly. A cache that filled
before it finished building is not served at all -- those scans fall back to
the gateway, so queries keep working at the cost of a round trip. It recovers
on its own once anything frees space.
