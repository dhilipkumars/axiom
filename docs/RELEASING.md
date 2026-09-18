# Releasing

How a version of Axiom is cut. Written for whoever is doing it, which today is
a maintainer with push access.

## The version lives in one place

`extension/Cargo.toml`'s `[package] version` declares it. Everything else
derives:

| Where | How |
|---|---|
| `extension/axiom.control` | pgrx templates `@CARGO_VERSION@` at package time |
| `axiom_version()` in SQL | `env!("CARGO_PKG_VERSION")` at compile time |
| the gateway binary | `-ldflags -X main.version`, from the image build's `VERSION` arg |
| the gateway's `Ping` reply | the same variable, so a client can see what it is talking to |

None of those is a second copy to keep in step, and `make release-check` fails
if one is turned into one. The gateway's default is `dev` on purpose: an
unstamped binary should say so rather than claim a release it is not.

To change the version, edit `extension/Cargo.toml` and nothing else.

## Cutting one

```sh
make release-check                  # changesets well-formed, version consistent
make changelog                      # read what the release will say
make release-notes VERSION=0.1.0    # assemble CHANGELOG.md, empty .changes/
```

Commit that, open it as a PR like anything else, and merge it. Then:

```sh
git tag v0.1.0 && git push origin v0.1.0
```

Then **publish a GitHub release from the tag**. That is the step that matters:
both `gateway-image.yml` and `postgres-image.yml` trigger on
`release: [published]`, not on the tag. A pushed tag with no published release
builds nothing.

`postgres-image.yml` does most of it: the per-major images, the extension
tarballs attached to the release, the install check that pulls the published
images with no credentials and runs the whole procedure against them, and only
then the floating tags.

The tag carries a leading `v`; the changelog heading and `make release-notes`
do not. `scripts/version check v0.1.0` enforces that they agree, and the
release workflow should call it before publishing anything.

## Writing the notes

Do not write them at release time. They are the `.changes/` files, written by
whoever made each change, when the reason is still fresh — see
[`.changes/README.md`](https://github.com/dhilipkumars/axiom/blob/main/.changes/README.md). `make release-notes` only
assembles them, in Keep a Changelog order, under a `## <version>` heading.

If the assembled notes read badly, fix the changesets and run it again; the
command refuses to write a section that already exists, so revert the file
first.

## Development images are not releases

`ghcr.io/dhilipkumars/axiom-gateway:development` and
`ghcr.io/dhilipkumars/axiom-postgres:development-pgNN` are built **nightly from
main**, and only when main has moved since the last run. A merge publishes
nothing on its own.

So a development tag means "main, as of last night", not "main, right now". If
you need the current head, run the *postgres image* or *gateway image* workflow
by hand — both accept `workflow_dispatch` — or build locally.

Nothing about a development image is a release: no version tag is written, and
`latest` is untouched. Only publishing a GitHub release moves those.

## What a release publishes

- **Container images**, per supported Postgres major, multi-architecture:
  `axiom-postgres:<version>-pgNN` and `axiom-gateway:<version>`, with the
  floating tags moved once the install check has passed against them.
- **Extension tarballs**, one per major and architecture, attached to the
  GitHub release with a `.sha256` beside each. These are for installing into a
  Postgres someone already runs; the images are for trying Axiom.

v0.1.0 predates the tarballs and has only images.
