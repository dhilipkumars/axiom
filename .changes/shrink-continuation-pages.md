---
kind: fixed
---

**Listing no longer fails when a later page is larger than the first.** On a
cluster whose objects vary widely in size -- CustomResourceDefinitions that
embed large schemas beside small ones, or ConfigMaps of very different sizes
-- a query could fail with `ResourceExhausted: ... cannot be split further`
even though every object fit. Only the first page of a listing could be made
smaller to fit a response; a later one was stuck with the first page's size.

A later page is now fetched again at a smaller size when it is too large, for
kinds served by Kubernetes itself (built-in kinds and CRDs), and every object
still arrives exactly once. This applies to watch-mode tables as well.
Aggregated APIs such as metrics-server keep the previous behaviour. A single
object too large for one response is still reported as before.

The gateway decides "served by Kubernetes itself" from the `apiservices`
object for the group, which the shipped RBAC can read. A deployment whose
RBAC cannot read it keeps the previous behaviour.
