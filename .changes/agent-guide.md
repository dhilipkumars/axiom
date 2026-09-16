---
kind: added
---

A setup guide written for coding agents, at
[guides/for-agents](https://dhilipkumars.github.io/axiom/guides/for-agents/):
the same path as getting started, but with a verification and expected output
for every step, failure signatures mapped to actions, idempotent commands, and
a single success criterion. Point an agent at it and it can bring up a working
environment without guessing.

The site also serves an [llms.txt](https://dhilipkumars.github.io/axiom/llms.txt)
index, following the emerging convention, so a model given the site root can
find the right page and the constraints that matter — RBAC bounds what a query
can reach, managed Postgres cannot run Axiom, images are amd64 only.
