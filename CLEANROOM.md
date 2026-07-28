# Clean-room policy

GridWork independently implements terminal multiplexing and agent orchestration —
domains where established projects carry copyleft (AGPL/GPL) licenses. This project is
Apache-2.0, and its independence is procedural, not just textual. This document is the
public record of that procedure, in force before any engine code exists.

## The rules

1. **No derived code.** Code copied, ported, or mechanically translated from any
   copyleft or otherwise incompatibly-licensed project is never accepted — from
   maintainers, agents, or external contributors (see CONTRIBUTING.md). Concept-level
   inspiration from public, documented behavior is fine; derivation is not.
2. **Engine authors don't read copyleft source.** Whoever implements code under the
   paths listed in `.github/cleanroom-paths.txt` (the PTY engine, engine adapters, and
   all multiplexer work) does not read the source of copyleft terminal multiplexers —
   before or during that work. Behavior is derived from public specifications and
   observed wire behavior only.
3. **Every non-obvious terminal behavior carries a derivation citation** — the public
   spec it implements (ECMA-48, XTerm ctlseqs, the Kitty keyboard protocol, ACP, …) or
   a captured observation registered in `docs/derivation/CAPTURES.md` by stable ID and
   SHA-256. A behavior with no citable permitted source is an escalation, not a guess.
4. **Independent second review.** Every change touching the clean-room paths gets an
   additional fresh-context review before it reaches `main`, checking exactly this:
   citations present and resolving, no source-derived framing, leak gate green. The
   review rides the public pull request.

## What this proves

Process, not similarity. Nobody on the reviewing side compares this codebase against
copyleft sources — doing so would itself breach rule 2. What the record shows is that
every behavior traces to a permitted source, and that the trail was written at commit
time rather than reconstructed after a question was raised.
