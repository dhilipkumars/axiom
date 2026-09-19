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

## What earns which number

Decide this from the rule, not from scratch at each release. The number is a
claim about what changed for someone using Axiom, so the test is always "what
can a user now do, or no longer do", never "how much work was it".

| Bump | For |
|---|---|
| **patch** — `0.1.0` → `0.1.1` | Fixes, packaging, distribution, tooling, docs, and security. Anything that does not change what Axiom can do. |
| **minor** — `0.1.1` → `0.2.0` | Meaningful capability: roadmap items, new SQL surface, new gateway behaviour. Grouped, and cut every few months rather than per merge. **While the major version is `0`, breaking changes land here too.** |
| **major** — `1.0.0` → `2.0.0` | A breaking change to a public surface, once there is a `1.0` to break. |

Three consequences are easy to get wrong:

**Shipping a capability is not the same as building one.** The extension
tarballs are the example: they made installing Axiom into an existing Postgres
possible without a Rust toolchain, which is a large improvement and still a
patch, because Axiom does exactly what it did before. New delivery of the same
functionality is mechanics. That is why the entry can be `kind: added` in the
changelog and still land in a patch release — the changelog describes the
change, the version describes the capability.

**While the major version is `0`, breaking changes go in the minor.** This is
Axiom's convention, not a rule semver imposes: semver says only that in `0.y.z`
"anything MAY change at any time", which would permit breaking something in a
patch. We do not, because there is no `1.0` yet, so major is not available as a
signal and bumping it would spend the one-time meaning of `1.0` — the
commitment that the surfaces below are stable — on an ordinary breaking change.
Minor is the loudest number left. A minor that breaks something says so at the
top of its changelog section, in the imperative, with the migration. After
`1.0`, breaking moves to major and this paragraph goes away.

**Adding to a surface is not breaking it.** A new FDW option, a new promoted
column, a new RPC: those are minors after `1.0` too. Only removing or
redefining something that already worked is a major.

### The two surfaces

Axiom has two, and they break differently:

- **The SQL surface** — server and user-mapping options, `IMPORT FOREIGN
  SCHEMA` options, promoted columns, and the `axiom_*()` functions. Breaking it
  breaks someone's queries and DDL.
- **The gRPC API** between the extension and the gateway. Breaking it means
  **the two halves must be upgraded together**, and that is not something a
  version number communicates on its own. Say it in the changelog entry, and
  say which direction of skew fails.

Everything under `deploy/` is an **example, not a surface**. Applying a renamed
manifest leaves the old objects in place and still working, so a rename there
does not break a running cluster and does not force a minor. It does oblige a
changelog entry with the cleanup commands and a warning about local edits —
`0.1.1`'s ClusterRole rename is the worked example. If a manifest change ever
*would* break a cluster that just re-applies it, that is a breaking change and
the paragraph above applies.

### Cadence

Minors go when enough has accumulated to be worth a release note, which in
practice is every few months. That is a rhythm, not a deadline: never ship an
empty minor to hit a date, and never rush half a feature into one. Patches go
whenever there is something to fix.

Security fixes are the exception to all of it — they ship as a patch
immediately, and are never held back for a cadence.

## Cutting one

```sh
make release-check                  # changesets well-formed, version consistent
make changelog                      # read what the release will say
make release-notes VERSION=X.Y.Z    # assemble CHANGELOG.md, empty .changes/
```

Commit that, open it as a PR like anything else, and merge it. Then:

```sh
git tag vX.Y.Z && git push origin vX.Y.Z
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
do not. `scripts/version check vX.Y.Z` enforces that they agree, and the
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

- **Container images**, multi-architecture: `axiom-postgres:<version>-pgNN`
  per supported major, and `axiom-gateway:v<version>` — note the gateway keeps
  the tag's leading `v` and the Postgres images do not.

  **Every floating tag moves on one piece of evidence.** `postgres-image.yml`
  promotes `latest`, `latest-pgNN` *and* the gateway's `latest`, in one step,
  and only after `install-check` has pulled the published images without
  credentials and run the whole install through against them — the gateway
  included, since that check deploys the released gateway image. So a release
  that does not install leaves every floating tag where it was, and two
  unpinned pulls give the pair that was tested together.

  `gateway-image.yml` publishes the version tag and stops there. It cannot
  promote: it knows both architectures built, which is not the same as knowing
  they work.
- **Extension tarballs**, one per major and architecture, attached to the
  GitHub release with a `.sha256` beside each. These are for installing into a
  Postgres someone already runs; the images are for trying Axiom.
- **`.deb` and `.rpm` packages**, the same six majors-and-architectures in two
  formats, also with a `.sha256` each. Built from the same exported tree as the
  tarball, so the library inside a package is the same bytes rather than a
  second compile. Prefer them wherever a package manager applies: they declare
  the glibc floor read off the binary, so an unsupported system is refused at
  install time instead of at the next postmaster start.

v0.1.0 predates the tarballs and has only images.

**v0.1.1 carries packages, but its own workflow did not build them.** They were
cut from that release's published tarballs after the fact and uploaded by hand,
so the bytes match — the library inside each package is byte-identical to the
one in the tarball — but re-running v0.1.1's workflow would not reproduce them.
From v0.1.2 the workflow produces all three formats itself. Do not repeat the
manual step; if a release is missing artifacts, fix the workflow and cut
another patch.
