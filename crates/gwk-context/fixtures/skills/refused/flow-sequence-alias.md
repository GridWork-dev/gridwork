---
name: flow-sequence-alias
description: A flow sequence carrying an alias rather than a plain scalar.
boom: [*a, *a, *a]
---

A flat flow sequence of plain scalars is accepted, because that is how upstream
spells `allowed-tools`. One carrying an alias, a tag, an anchor, or another
collection is not: those are the shapes the sequence would otherwise smuggle
past the head-of-node scan.
