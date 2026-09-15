# Changesets

A change that a user would notice gets a note in this directory.
`make release-notes VERSION=x.y.z` assembles them into `CHANGELOG.md` and
empties this directory; `make changelog` renders what is pending without
consuming it, which is the quickest way to check how your entry reads next to
everyone else's. See [docs/RELEASING.md](../docs/RELEASING.md).

## Writing one

Add a file named `<pr-number-or-slug>.md`:

```markdown
---
kind: added | changed | fixed | removed | security
---

`IMPORT FOREIGN SCHEMA` now accepts a `prefix` option, so two clusters can be
imported into one schema without their table names colliding.
```

Write it for someone using Axiom, not for someone reviewing the diff. "Fixed
the off-by-one in `assign_table_names`" is a commit message. "Two kinds whose
plural names collide now get distinct table names instead of the import
failing" is a changeset.

## When to skip one

Most changes do not need one. Refactors, test changes, dependency bumps,
documentation, and anything invisible from SQL or from a gateway's command line
all skip it. Say so in the PR description, or apply the `no-changeset` label.

## What CI does and does not enforce

**Whether a change needed a changeset is not enforced, deliberately.** A rule
that every code change must add a file false-positives on every refactor and is
satisfied by an empty file, so it trains people to add noise rather than to
write changelog entries. Whether a change is user-visible is a judgement, and
judgement belongs in review.

**Whether a changeset that exists is well-formed is enforced**, by
`make release-check`. That is not a judgement: a `kind:` outside the five above
drops the entry from the changelog silently, and the way you find out is by
noticing it missing after the release. An entry with frontmatter and no body is
rejected for the same reason. The distinction is the one below.

The generated reference under `docs/generated/` is the opposite case: drift
there is mechanically detectable, so `make docs-check` enforces it in CI and no
one has to notice.
