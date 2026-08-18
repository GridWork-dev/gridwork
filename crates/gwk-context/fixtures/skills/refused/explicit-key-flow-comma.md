---
name: explicit-key-flow-comma
description: The after-comma spelling of a flow explicit key.
first: [x, ?*anc]
---

A comma reopens a node start inside a flow collection, so the parser's bare-`?`
relaxation fires mid-list exactly as it does at the opening bracket. The
corpus's only explicit-key coverage was the block form, where the blank rule is
the parser's own — which is why six rounds of differential testing never put a
`?` behind a `[` or a `,`.
