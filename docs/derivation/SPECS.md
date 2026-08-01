# Permitted specification registry

Public specifications cited by clean-room derivation markers (CLEANROOM.md rule 3).
Rule 3 admits two kinds of permitted source: a public specification, or an observation
registered in `CAPTURES.md`. This file is the first kind; that file is the second.

A `Derivation:` marker cites an ID from one of these two tables, and `cleanroom-gate`
rejects a citation that resolves in neither — so rule 3 is enforced by the gate rather
than by a reviewer ticking a box.

Cite as `<ID> §<section>` where the spec has sections: `ECMA-48 §8.3.14`. The section is
for the reader; the gate resolves the ID.

**A row is scoped to the document it names, and a citation may only claim what that
document says.** POSIX is why this is spelled out: `POSIX-TERM` is XBD Chapter 11, which
describes the terminal interface but defines no function, so a fact about what
`tcsetwinsize()` does or when it signals belongs to `POSIX-WINSIZE` — a different volume,
a different row. Citing the chapter for the function page resolves happily through the
gate and is exactly the wrong-section failure rule 4 exists to catch; it got through three
review rounds on this crate before a reader searched the chapter text instead of trusting
that a plausible section number was the right one.

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
| `CLAUDE-STREAM-JSON` | Claude Code headless interface: stream-json input/output message shapes | code.claude.com/docs/en/headless |
| `CLAUDE-HOOKS` | Claude Code hooks reference: lifecycle events, PreToolUse `permissionDecision` contract | code.claude.com/docs/en/hooks |
| `CODEX-APP-SERVER` | Codex `app-server` JSON-RPC protocol: README plus the schemas emitted by `codex app-server generate-json-schema` | github.com/openai/codex/blob/main/codex-rs/app-server/README.md |
| `OPENCODE-SERVER` | opencode server HTTP + SSE surface: session, permission, and question events and their reply endpoints | opencode.ai/docs/server |
| `POSIX-TERM` | POSIX.1-2024 (Issue 8) XBD §11, General Terminal Interface | pubs.opengroup.org/onlinepubs/9799919799/basedefs/V1_chap11.html |
| `POSIX-WINSIZE` | POSIX.1-2024 (Issue 8) XSH, `tcsetwinsize()` | pubs.opengroup.org/onlinepubs/9799919799/functions/tcsetwinsize.html |
| `UAX-11` | Unicode Standard Annex #11, East Asian Width | unicode.org/reports/tr11/ |
| `UAX-29` | Unicode Standard Annex #29, Text Segmentation (grapheme clusters) | unicode.org/reports/tr29/ |

## Rows exist before the code that cites them

This registry is seeded ahead of `gwk-pty` deliberately. A registry written to match code
already merged records what was cited, not what was permitted — and the whole value of
rule 3 is that the permitted set is decided before the implementation reaches for it.

Nothing here is load-bearing until a marker cites it. A row nothing cites costs nothing;
a missing row blocks a PR until someone justifies the source, which is the correct
direction for that friction to run.

Once a marker does cite a row, the row is load-bearing and `cleanroom-gate` treats it
that way: cited row text is folded into the review subject, and editing a cited row
re-opens review of the files citing it (`CAPTURES.md` describes the mechanism; it covers
both registries).
