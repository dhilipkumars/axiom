---
kind: security
---

Updated rustls to 0.23.45, which fixes RUSTSEC-2026-0285: TLS 1.3 handshake
messages were accepted across encryption level boundaries. rustls terminates
the extension's side of the Postgres-to-gateway connection, so this sits on the
boundary that carries every cluster read and write.
