---
name: block-scalar-in-sequence-item
description: An anchor and its alias hidden behind a block scalar in a sequence item.
aa:
  - k: |
      t
    m: &zz SECRET
bb:
  - k: |
      t
    m: *zz
---

The block scalar skip was armed with the header LINE's indentation, which is the
column of the node owning the scalar only when nothing on that line moved it
right. A `- ` marker does. Here the header line indents 2 while the mapping
holding `k` starts at column 4, and the parser ends the scalar at the content
indentation it detects — 8. A sibling key at column 4 then satisfies 4 > 2, so
this scan skipped it as content, and 4 < 8, so the parser read it as YAML.

`SECRET` is written once, under `aa`. It came back under `bb`, which wrote only
`*zz`.

Column zero is the one position where the line indent and the owning node's
column coincide, and every block scalar in the corpus sat there — so no fixture
and no unit test could observe this shape. Both spellings of the header reach it
identically; the `>` twin is asserted in the unit tests beside this file.
