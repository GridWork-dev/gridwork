---
name: explicit-key
description: An anchor after the other block-context node opener.
aa:
  ? &s SECRET
  : 1
bb:
  ? *s
  : 2
---

`- ` gets an arm because it opens a node. `? ` opens one too and had none: it
fell to the catch-all that clears `node_start`, and the whitespace skip does not
restore it, so the anchor it introduces was never examined. Its sibling `: `
line refuses only by luck — a `:` at the head hits the key-terminator arm, which
sets `node_start` back to true.

No portable manifest uses an explicit key, so this is refused outright rather
than measured. The alias resolved across two top-level keys here as well, and
in the `metadata:` spelling the anchored value reaches typed output rather than
opaque evidence.
