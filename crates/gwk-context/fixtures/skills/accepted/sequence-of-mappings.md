---
name: sequence-of-mappings
description: A block sequence whose entries are mappings, more than one of them.
authors:
  - name: a
    role: writer
  - name: b
contributors:
- name: c
- name: d
---

Every accepted fixture beside this one holds sequences of plain scalars, so the
corpus could not tell a depth bound from an item counter. This one was refused —
the whole manifest, not the field — because the level a mapping opens inside a
sequence item was pushed at the line's indentation, where no later line could
pop it. One entry measured two levels, two entries measured four.

That made an unrecognized portable field fatal, which the module header rules
out: upstream shipping a new field is not an attack, and `authors:` is what the
ordinary spelling of one looks like. The second key on the first entry and the
column-zero spelling below it are the two neighbouring shapes that failed the
same way.
