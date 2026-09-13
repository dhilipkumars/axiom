---
kind: changed
---

**Breaking: Postgres 14 and 15 are no longer supported.** The supported window
is now 16 through the latest major, currently 16, 17 and 18, and every one of
them is built and tested in CI.

14 reaches end of life in November 2026 and 15 is not where the installed base
sits; 16 is supported upstream until November 2028. Building the extension now
requires Rust 1.96 or newer, because it moved to pgrx 0.19.
