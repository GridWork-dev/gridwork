---
name: tab-separator
description: A tab where the key separator's space should be.
extra:	&a hello
---

The tab twin of `refused/anchor.md`. A tab is exempt from the control-character
refusal and the indentation check only reads the leading run, so this line held
no `": "` anywhere — and with no separator to split on, the candidate node fell
back to the entire line, whose first character is `e`.
