---
kind: security
---

The gateway is built with Go 1.27.2 and `golang.org/x/net` v0.60.0, which fix
nine advisories in HTTP/2, TLS and HTTP handling (GO-2026-6617, GO-2026-6613,
GO-2026-6612 and others). The gateway serves gRPC over HTTP/2 and reaches the
API server over it, so all of them were on a path the gateway uses.
Upgrade the gateway image; the extension is unaffected.
