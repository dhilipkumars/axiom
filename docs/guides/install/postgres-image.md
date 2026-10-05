# Postgres image

The fastest way to get a Postgres with Axiom: an image with the extension
installed and preloaded.

```sh
docker run -d --name axiom-postgres \
  -e POSTGRES_PASSWORD=change-me \
  -v "$PWD/certs:/certs:ro" \
  -p 127.0.0.1:55432:5432 \
  ghcr.io/dhilipkumars/axiom-postgres:latest-pg17
```

- **`-v "$PWD/certs:/certs:ro"`** puts the gateway's `ca.crt` where Postgres can
  read it. The `ca_cert` server option is a path on the *Postgres server's*
  filesystem, so inside this container it is `/certs/ca.crt`.
- **`-p 127.0.0.1:55432:5432`** is for your own `psql`, and only on this
  machine. `docker exec -it axiom-postgres psql -U postgres` works without it.
- **On a kind cluster**, add `--network kind`. Postgres then reaches the
  gateway's NodePort at `<cluster>-control-plane:30443` directly, with no port
  mapping on the cluster. Every kind cluster shares that one network.

The image is the official `postgres` image with Axiom added, so everything its
[documentation](https://hub.docker.com/_/postgres) describes, such as
`POSTGRES_PASSWORD`, volumes and init scripts, applies unchanged.

## Tags

| Tag | Follows |
|---|---|
| `latest-pg16`, `latest-pg17`, `latest-pg18` | the newest release, for that major |
| `0.2.0-pg17` (and so on) | one release; never moves |
| `latest` | the newest release, on the newest major |

Pin a version in anything you keep, and move it when you choose to. Every tag
is published for amd64 and arm64.

`shared_preload_libraries = 'axiom'` is already set, which is the one thing a
package cannot do for you. Next: [initialize](../initialize.md).
