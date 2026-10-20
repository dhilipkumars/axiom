---
kind: security
---

The extension uses rustls 0.23.46, which fixes four advisories
(RUSTSEC-2026-0346 to RUSTSEC-2026-0349). None is reachable in Axiom's
configuration: each needs a rustls feature or setting Axiom does not use
(Brotli certificate compression, the aws-lc-rs post-quantum key exchange,
Encrypted Client Hello, or a custom session store). The update keeps the
extension off a flagged release.
