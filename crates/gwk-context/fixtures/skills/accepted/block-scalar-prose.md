---
name: block-scalar-prose
description: A block scalar whose content begins with indicator characters.
note: |
  *bold* at the start of a line, &c as an entity, !important as emphasis.
  ---
  - a
    - b
      - c
---

The positive control the refusals need from the other side. A block scalar's
content is a string to the parser, and scanning it as YAML refused all of this:
`*bold*` as an alias, `&c` as an anchor, `!important` as a tag, `---` as a
document marker, and the nested bullets as nesting past the depth bound.

Every one of those is ordinary prose in a manifest, and each refusal put
pressure on the indicator list — which is the one place relieving it would
reopen the escapes the list exists to close. The content is skipped instead,
ending where the indentation returns to the header's, which is the rule the
parser applies too.
