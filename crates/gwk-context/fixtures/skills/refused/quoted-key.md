---
"x: y": plain
name: quoted-key
description: A quoted top-level key whose own text contains a key separator.
---

Deliberately inert on the right-hand side, so only the key can refuse it. The
separator inside the quotes is where the split used to land, which both hid
whatever followed the real separator and mangled the evidence record: the field
came back keyed `"x` with an empty value, because the later lookup searched the
document for a key that was not in it. A silently dropped field is the one
outcome this module's header rules out.

Quoted top-level keys are refused rather than repaired — legal YAML that no
portable manifest uses, and admitting it costs more than it is worth.
