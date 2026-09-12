---
kind: added
---

`axiom_gateway_stats('<server>')` reports a gateway's per-RPC call counters, so
a cache-served scan can be told from an on-demand one without reading the
gateway's log. The counters are per-process and reset when the gateway
restarts.
