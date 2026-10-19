---
kind: security
---

The extension uses rustls 0.23.46, which fixes four advisories in the TLS
library the extension connects to the gateway with (RUSTSEC-2026-0346 to
RUSTSEC-2026-0349): a ServerHello that could downgrade later connections to
classical key exchange, certificate decompression that could allocate up to
1 GiB before authentication, `Debug` output that exposed session secrets, and
buffered data sent after an ECH rejection. Upgrade the extension; the gateway
is unaffected.
