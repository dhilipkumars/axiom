# Changesets

A change that a user would notice gets a note in this directory. A release
assembles them into the changelog.

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

## Why this is not enforced by CI

It deliberately is not. A rule that every code change must add a file
false-positives on every refactor and is satisfied by an empty file, so it
trains people to add noise rather than to write changelog entries. Whether a
change is user-visible is a judgement, and judgement belongs in review.

The generated reference under `docs/generated/` is the opposite case: drift
there is mechanically detectable, so `make docs-check` enforces it in CI and no
one has to notice.
