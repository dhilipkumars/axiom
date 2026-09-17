---
kind: fixed
---

The first query after about 40 seconds of an idle session no longer fails with
`cannot reach gateway ... transport error`. The gateway pings its peers and
drops one that does not answer within ten seconds, and a Postgres backend
sitting between queries has nothing running to answer with — so the gateway
closed a connection the backend still had cached, and the next query paid for
discovering it.

That query is now retried once on a fresh connection. Inside a transaction the
old behaviour aborted the transaction and lost the work, with no way to retry;
for a pooled client it happened on every gap longer than the keepalive window.

Only reads are retried. A transport error does not say whether the gateway saw
the request, and a replayed `delete` answers `NotFound`, which cannot be told
apart from "it was never there" — so writes still fail once rather than risk
reporting a successful write as an error.
