---
name: unknown-portable-field
description: Upstream shipped a field this parser predates.
invocation-hint: proactive
---

`invocation-hint` is not in the exhaustive field set this parser knows. It is
kept as opaque evidence rather than dropped or refused: legitimate upstream
drift is not an attack, and a dropped field is a silent difference between what
the author wrote and what the compiler saw.
