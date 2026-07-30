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
   spec it implements or a captured observation, cited by stable ID from
   `docs/derivation/SPECS.md` or `docs/derivation/CAPTURES.md`. The citation is a
   `Derivation:` marker on the line above the derived construct:

   ```
   // Derivation: ECMA-48 §8.3.14 — cursor save/restore semantics
   ```

   `cleanroom-gate` enforces this mechanically: every gated source file carries at least
   one marker, and every cited ID resolves in one of the two registries. Never cite a
   source by path — rule 4's `references` check bans it, and the gate rejects it.

   A behavior with no citable permitted source is an escalation, not a guess.

   What the gate proves is that the claim was made and that the source is registered.
   It does not prove the claim is TRUE — a marker can name a spec the code did not come
   from, and only rule 4's reader catches that. The gate makes a false citation
   attributable; the review is what makes it unlikely.
4. **Independent second review.** Every change touching the clean-room paths gets an
   additional fresh-context review before it reaches `main`, checking exactly this:
   citations present and resolving, no source-derived framing, no other project named as
   a comparand and no capture cited by path, leak gate green. The
   reviewer is a fresh-context session with no exposure to the implementing session —
   **not a second human**: this repository has one maintainer, and claiming a control it
   cannot deliver would be worse than stating the narrower one plainly. The review is
   recorded in `docs/derivation/reviews/`, bound by digest to the exact content it read,
   and the `cleanroom-gate` check enforces that the record exists and matches.

## What this proves

Process, not similarity. Nobody on the reviewing side compares this codebase against
copyleft sources — doing so would itself breach rule 2. What the record shows is that
every behavior traces to a permitted source, and that the trail was written at commit
time rather than reconstructed after a question was raised.

What it does not show is reviewer independence. No status check can establish that, and
rule 4 says what the reviewer actually is instead of implying more.
