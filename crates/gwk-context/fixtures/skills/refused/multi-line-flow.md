---
name: multi-line-flow
description: An anchor on the continuation line of an open flow collection.
aa: [
  0, &s SECRET]
bb: [
  0, *s]
---

Every fixture in this corpus was one line, so every refusal it recorded was a
refusal of a first line. `scan_line` is handed one line with its state reset,
and this collection opens on one and continues on the next: on the second line
`flow` reads zero, so the `,` cleared `node_start` rather than setting it, and
the indicator branches that refuse an anchor went dead.

The value written once under `aa` came back under `bb` — libyaml resolved the
reference. The single-line spelling of exactly this document was already
refused; the line break was the whole escape, and no test in the suite could
tell the two apart.
