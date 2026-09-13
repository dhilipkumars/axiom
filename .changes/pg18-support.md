---
kind: added
---

Postgres 18 is supported. Two upstream changes needed handling: the planner's
`create_foreignscan_path` gained a `disabled_nodes` argument, and tuple
descriptors replaced their inline attribute array with compact attributes, so
reading a column's name and type goes through a version-gated accessor.
