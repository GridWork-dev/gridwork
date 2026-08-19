---
name: flow-in-sequence-item
description: A flow mapping behind one inert leading pair.
x:
  - {u: 1, v: &a hello}
---

`refused/flow-mapping.md` claims in its own prose to cover the case where the
indicator "is not at the head of the node", and it did not: it puts the
collection in the top-level `key: <flow>` position, where the first `": "`
belongs to the top-level key and a head test lands on the brace by positional
luck. Move the collection into a sequence item and give it one leading pair and
the first `": "` becomes `u`'s, so the candidate node started `1, v: &a hello}`
and the scan examined a digit.

This is the shape that put an anchor and its alias through to the transpiled
parser and had them resolved.
