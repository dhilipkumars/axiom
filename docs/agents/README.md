# Delegation briefs for the `agy` CLI

Axiom's Claude Code sessions delegate some work to the `agy` (Antigravity) CLI,
running Gemini. These files are the standing context for that, so a brief does
not have to be retyped every time.

Use the wrapper, which picks the model, pins the working directory and passes
the brief:

```sh
scripts/agy-role architect "Review docs/AUTH.md for ..."
scripts/agy-role junior    "Add a test that ..."
scripts/agy-role worker --timeout 1800s "Run make e2e and report"
```

| Brief | Model | Use for |
|---|---|---|
| [architect.md](./architect.md) | `Gemini 3.8 Flash (Medium)` | design review, adversarial brainstorming |
| [junior.md](./junior.md) | `Gemini 3.8 Flash (Low)` | mechanical edits, scaffolding, small changes |
| [worker.md](./worker.md) | `Gemini 3.8 Flash (Low)` | long-running jobs; reports facts, changes nothing |

## Why a wrapper and not agy's own agent/skill mechanism

**Nothing project-local is auto-loaded.** Tested against agy 1.2.2, in a scratch
git repo and in this one, in print mode:

| Tried | Result |
|---|---|
| `AGENTS.md`, `GEMINI.md` | not read |
| `.agents/rules/*.md` | not read |
| `.agents/agents/<name>.md` with `--agent <name>` | not read; `--agent` with a nonsense name is silently ignored |
| `.agents/skills/<name>/SKILL.md`, and the same under `.gemini/` and `.agy/` | not read |
| `~/.agents/rules/*.md` | not read |

`agy` itself claimed it reads `AGENTS.md` and `GEMINI.md`, which it does not —
so its self-report is not a reliable guide here.

Skills *are* discovered from `~/.gemini/config/plugins/<plugin>/skills/<name>/SKILL.md`,
note the `<name>/SKILL.md` directory shape rather than `<name>.md`. That is
user-level, and this project deliberately keeps no user-level configuration, so
the wrapper passes the brief inline instead.

Two mechanics the wrapper handles so callers do not have to:

- **Effort is part of the model name.** `--effort` is rejected for these models.
- **`--dangerously-skip-permissions` is required.** Headless mode cannot prompt,
  so without it every tool call is auto-denied and the run produces nothing.
- **The working directory is not the repository root.** A bare `make
  gateway-test` failed with "No rule to make target" until the root was stated
  explicitly.

**Verify anything load-bearing that comes back.** In practice these reviews mix
correct findings with overstated ones. That is still worth the cost — an
architect-role review of `docs/AUTH.md` found a real privilege-escalation hole
that empirical testing then confirmed — but the verification is not optional.
The same session saw `agy` confidently misreport which context files it reads.
