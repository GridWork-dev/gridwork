# Permitted specification registry

Public specifications cited by clean-room derivation markers (CLEANROOM.md rule 3).
Rule 3 admits two kinds of permitted source: a public specification, or an observation
registered in `CAPTURES.md`. This file is the first kind; that file is the second.

A `Derivation:` marker cites an ID from one of these two tables, and `cleanroom-gate`
rejects a citation that resolves in neither — so rule 3 is enforced by the gate rather
than by a reviewer ticking a box.

Cite as `<ID> §<section>` where the spec has sections: `ECMA-48 §8.3.14`. The section is
for the reader; the gate resolves the ID.

**Every row is a public, freely-obtainable document.** That is the point of the registry —
a source that cannot be read by anyone auditing this repo cannot substantiate a claim of
independent derivation. Adding a row is a normal PR; adding one that is paywalled,
leaked, or copyleft-licensed is a rule 1 violation.

| ID | Specification | Where |
|---|---|---|
| `ECMA-48` | Control Functions for Coded Character Sets, 5th edition (1991) | ecma-international.org/publications-and-standards/standards/ecma-48/ |
| `XTERM-CTLSEQS` | XTerm Control Sequences | invisible-island.net/xterm/ctlseqs/ctlseqs.html |
| `KITTY-KBD` | Kitty keyboard protocol | sw.kovidgoyal.net/kitty/keyboard-protocol/ |
| `KITTY-GRAPHICS` | Kitty graphics protocol | sw.kovidgoyal.net/kitty/graphics-protocol/ |
| `ACP-1` | Agent Client Protocol, wire version 1 | agentclientprotocol.com |
| `POSIX-TERM` | POSIX.1-2024 §11 General Terminal Interface | pubs.opengroup.org/onlinepubs/9799919799/ |
| `UAX-11` | Unicode Standard Annex #11, East Asian Width | unicode.org/reports/tr11/ |
| `UAX-29` | Unicode Standard Annex #29, Text Segmentation (grapheme clusters) | unicode.org/reports/tr29/ |

## Rows exist before the code that cites them

This registry is seeded ahead of `gwk-pty` deliberately. A registry written to match code
already merged records what was cited, not what was permitted — and the whole value of
rule 3 is that the permitted set is decided before the implementation reaches for it.

Nothing here is load-bearing until a marker cites it. A row nothing cites costs nothing;
a missing row blocks a PR until someone justifies the source, which is the correct
direction for that friction to run.
