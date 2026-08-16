---
name: flow-mapping
description: A flow mapping hides its nodes from a head-of-node scan.
extra: {v: &a hello}
---

The anchor here is not at the head of the node, so the indicator checks that
refuse `description: &a d` never see it. The flow mapping is refused as a shape
for that reason, and because it is the form that nests.
