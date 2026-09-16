---
kind: fixed
---

Installing Axiom without `shared_preload_libraries` now says so. It previously
failed with `FATAL: cannot create PGC_POSTMASTER variables after startup`,
which names neither Axiom nor the setting that fixes it, and which took the
client's connection down with it.

It is now an ordinary error naming the extension, the setting, and the restart:
the statement fails and the session stays open.

```
ERROR:  axiom must be loaded through shared_preload_libraries
DETAIL:  Add `shared_preload_libraries = 'axiom'` to postgresql.conf, restart
Postgres, then run CREATE EXTENSION axiom. ...
```

Nothing changes for a correctly preloaded Postgres, which includes the
published images — they preload Axiom themselves.
