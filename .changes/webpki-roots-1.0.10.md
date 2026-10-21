---
kind: security
---

**Extension: webpki-roots 1.0.10 and rustls-webpki 0.103.16**, fixing
RUSTSEC-2026-0381 and RUSTSEC-2026-0382. Both are about trusting certificates
that chain to distrusted emSign intermediates, whose private keys are now
public. They matter for a server with no `ca_cert`: the extension then
verifies the gateway against these bundled public roots, so a certificate
from one of those intermediates could impersonate the gateway. A server with
`ca_cert` set trusts only that CA and was not exposed.
