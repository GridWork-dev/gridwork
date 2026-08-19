---
name: deep-nesting
description: Block nesting one level past the depth bound.
metadata:
  a:
    b:
      c: 1
---

`SKILL_MAX_NESTING_DEPTH` had no fixture and no test: deleting the branch that
enforces it left the suite green, which made the bound decoration. This is the
case that notices.
