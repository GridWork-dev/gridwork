---
name: flow-comment-after-quote
description: The same comment hole opened one position later, after a quoted scalar.
first: [a, "b"#]
  , &anc SECRET, *anc]
---

A closed quoted scalar is a token boundary the node-start model does not mark:
a node may not open there, but the parser is between tokens all the same, so
the `#` is a comment to it here exactly as it is after `[` or `,`. The
continuation opens with the comma the quoted entry still owes, which is why
this spelling survived a fix aimed only at the positions a node can start —
and why the boundary the scan tests is the parser's, not the node's.
