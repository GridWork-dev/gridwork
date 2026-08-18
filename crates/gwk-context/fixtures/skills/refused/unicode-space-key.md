---
name: unicode-space-key
description: A no-break space smuggling a key past its own name.
 metadata:
  team: hidden
---

The character before `metadata:` is U+00A0. The indentation run is measured in
ASCII and the parser agrees — a no-break space is content to it, so the
document's key is `\u{a0}metadata` — but `str::trim` strips the whole Unicode
White_Space class, so the scan looked the field up under a name the document
does not contain. The real field vanished from the record and the GridWork
namespace's loud refusal was skipped silently: an unknown `metadata.gridwork`
key sailed past the one gate built to reject it by name. The same character one
byte later was already refused, so acceptance here was an inconsistency, not a
policy.
