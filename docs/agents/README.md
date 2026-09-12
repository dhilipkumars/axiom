# Delegation briefs for the `agy` CLI

Axiom's Claude Code sessions delegate some work to the `agy` (Antigravity) CLI,
running Gemini. These files are the standing context for that, so a brief does
not have to be retyped every time.

**They are not auto-loaded.** `agy` reports that it reads `AGENTS.md` or
`GEMINI.md` from a repository, but that was tested and it does not — neither in
a scratch directory nor in this repo, in print mode. Pipe the brief in instead:

```sh
agy -p "$(cat docs/agents/junior.md)

TASK: <the specific thing>" \
  --model "Gemini 3.8 Flash (Low)" \
  --dangerously-skip-permissions --print-timeout 300s
```

`--dangerously-skip-permissions` is required: headless mode cannot prompt and
auto-denies every tool call without it.

| Brief | Model | Use for |
|---|---|---|
| [architect.md](./architect.md) | `Gemini 3.8 Flash (Medium)` | design review, adversarial brainstorming |
| [junior.md](./junior.md) | `Gemini 3.8 Flash (Low)` | running known commands, mechanical edits, scaffolding |

**Verify anything load-bearing that comes back.** In practice these reviews mix
correct findings with overstated ones. That is still worth the cost — an
architect-role review of `docs/AUTH.md` found a real privilege-escalation hole
that empirical testing then confirmed — but the verification is not optional.
The same session saw `agy` confidently misreport which context files it reads.
