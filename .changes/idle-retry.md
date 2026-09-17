---
kind: fixed
---

The first query after about 40 seconds of an idle session no longer fails with
`cannot reach gateway ... transport error`. The gateway pings a peer that has
been idle for 30 seconds and drops it 10 seconds later, and a Postgres backend
sitting between queries has nothing running to answer with — so the gateway
closed a connection the backend still had cached, and the next statement paid
for discovering it.

A cached connection is now rebuilt once it has been unused for 25 seconds,
before the gateway has even started asking. Reads and writes alike: previously
a transaction that paused and then wrote lost the write and aborted, with no
way to retry, and a pooled client hit this on every gap longer than the
keepalive window.
