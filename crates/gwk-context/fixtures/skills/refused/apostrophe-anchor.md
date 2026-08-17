---
name: apostrophe-anchor
description: An anchor behind an apostrophe in a plain scalar.
x: [don't, &a hello]
y: [don't, *a]
---

YAML permits `'` and `"` inside a plain scalar and forbids them only at its
head, so `don't` keeps the parser in a plain scalar exactly where this scan
entered quote state. Everything after the apostrophe was then treated as quoted
text, and quoted text is the one region the indicator branches do not examine.

The control differs by one character: `x: [1, &a hello]` refuses as an anchor.
The apostrophe is the entire mechanism, and the alias in `y` was resolved to the
value anchored in `x`.
