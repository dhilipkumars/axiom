---
kind: fixed
---

**Listing large objects makes fewer API calls.** When a page of a listing was
over the gateway's size limit, the gateway asked again for half as many
objects as it had *requested*. On the last page of a collection, which holds
fewer, that fetched the same large objects again at each step: four 1.5 MiB
ConfigMaps at the end of a listing cost eight List calls. It now halves what
came back, which takes two; a single object too large to send costs one call
instead of two.
