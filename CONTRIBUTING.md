# Contributing

Thanks for looking. Axiom is a Postgres foreign data wrapper for Kubernetes —
a pgrx extension and a Go gateway — so a change usually touches Rust, Go, or
the boundary between them.

## Before you write code

- **Build and run it first.** [docs/development.md](docs/development.md) has the
  toolchain, the local stack, and the E2E gates. `make e2e` brings up a kind
  cluster and exercises every capability end to end; it is the fastest way to
  see what the project actually does.
- **Read [docs/RULES.md](docs/RULES.md).** It is short, and it is the standard a
  change is held to rather than a statement of aspiration.
- **Open an issue for anything non-obvious.** Design discussion is cheaper
  before the code than after.

## What a change needs

- **`make lint unit` green locally.** CI runs the same, plus `make ext-audit`,
  `govulncheck`, gitleaks and the E2E gates.
- **Tests for the failure paths**, not only the happy path. This project has
  more than once found a gate that was green because it was not testing
  anything — a refusal that never fired, an assertion satisfied for the wrong
  reason. If a test would pass with the feature deleted, it is not a test.
- **A changeset** if a user would notice the change. See
  [`.changes/README.md`](.changes/README.md) for what counts and what does not.
  Whether one was needed is a judgement made in review; whether one that exists
  is well-formed is enforced by `make release-check`.
- **Regenerated reference pages** if you changed a `.proto` comment, an FDW
  option or a column rule: `make docs-generate`, and commit the result. CI fails
  on drift.

## Commit messages and PRs

Say what changed and **why it was wrong before**. The commit log is the main
record of reasoning in this repository, and a message that only restates the
diff throws that away. If you fixed something subtle, the message is where the
subtlety lives.

## Review

Every PR gets an adversarial review before merge, and findings are verified
against the code rather than applied on sight — a review that is wrong about
something should be said to be wrong, with the reason.

## Licence

By contributing you agree your work is licensed under Apache-2.0, as in
[LICENSE](LICENSE).
