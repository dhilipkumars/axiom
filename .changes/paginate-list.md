---
kind: fixed
---

A scan of a large collection no longer fails outright. `List` is now paged, so
a table with more objects than fit in one gRPC message is read across several
requests instead of exceeding the 4 MiB default and erroring. Both ends now set
an explicit 16 MiB message limit as a backstop, and a page is bounded by bytes
as well as by object count, because a count that suits Pods can be hundreds of
megabytes of ConfigMaps.

A watch subscription's initial listing is paged the same way, so starting a
watch on a large kind no longer pulls the whole collection into gateway memory
before the first event.
