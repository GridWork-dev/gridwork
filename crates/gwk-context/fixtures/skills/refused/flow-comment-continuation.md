---
name: flow-comment-continuation
description: A comment closing a flow collection for the scan and not the parser.
first: [Read,#]
  X, &anc SECRET, *anc]
---

The parser skips to its next token before every fetch and starts a comment at a
bare `#` wherever it lands — no preceding blank required — so the `]` on the
first line is inside a comment and the flow collection is still open when line
two is scanned. A rule that opened a comment only after a blank read the same
line as a plain scalar, consumed that `]`, and ended the line at depth zero, so
the line-local refusal never fired; the continuation was then rescanned as a
fresh block line, where `,` clears the node start and the anchor and alias
behind it are never examined. The parser RESOLVED the alias into the sequence.
Refused as an unclosed flow collection, which is what the first line is.
