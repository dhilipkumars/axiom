---
kind: security
---

**Gateway: Go 1.27.4**, fixing GO-2026-6859 (CVE-2026-97054), a race in
`net/http`'s HTTP/1 server when `MaxBytesReader` closes a connection. The
gateway runs no HTTP/1 server, only gRPC, so it was not reachable; the
toolchain is updated so the vulnerability scan stays clean.
