---
name: explicit-key-flow
description: An anchored explicit key inside a flow collection.
first: [?&anc SECRET]
second: [?*anc]
---

The block spelling of an explicit key needs a blank after its `?`; inside a
flow collection the parser's dispatch table drops that requirement and takes
bare `?` as a KEY token. So the scan read a plain scalar where the parser read
an anchored key, the alias under `second` RESOLVED to content written under
`first`, and both sat at depth one — one indicator over from the quoted-key
spelling of the same escape. Refused at the `?` itself, which is why this
file's matcher names the indicator rather than the anchor behind it.
