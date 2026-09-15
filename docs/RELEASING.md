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
`gateway-image.yml` triggers on `release: [published]`, not on the tag, and
that is what publishes the semver image tag and moves `:latest`. A pushed tag
with no published release builds nothing.

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

## What a release does not do yet

Publish downloadable extension artifacts. Today a release publishes container
images, which serve evaluation rather than installation into an existing
Postgres — see the tracking issue for v0.1.0 and the artifact issue it defers.
