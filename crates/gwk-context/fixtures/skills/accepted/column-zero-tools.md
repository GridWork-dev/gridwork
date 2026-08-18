---
name: column-zero-tools
description: A column-zero block sequence that is not the last construct.
allowed-tools:
- Read
- Grep
metadata:
  team: infra
---

Every other column-zero sequence in the corpus is the last construct in its
document, so a level that could never be closed looked exactly like a level
nothing needed to close. This one is followed by another top-level key opening
a mapping — the shape that was refused as TooDeeplyNested at a true depth of
one, with every key portable core.
