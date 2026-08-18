---
name: quoted-flow-key-alias
description: An alias behind a JSON-style quoted flow key.
second: ["b":*anc]
---

The parser takes `:` after a quoted flow key as a key indicator with no space
required, so the alias here was never at what the scan considered a node
start. With its anchor defined under an earlier key, the pinned parser
RESOLVED it — one key's evidence carrying content written under another —
which is the exact property the gate exists to stop.
