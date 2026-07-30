subject: <paste the digest from `./tools/cleanroom-gate.sh --subject`>
reviewer: <the reviewing lane — a fresh-context session with no exposure to the implementing session>
date: <YYYY-MM-DD>

# Second-reader record

## Scope

<!-- The gated paths reviewed, and what the change does to them, in a sentence. -->

## Checks

- [ ] citations — every non-obvious terminal behavior cites a public specification or a
      `docs/derivation/CAPTURES.md` entry by ID, and every cited ID resolves
- [ ] framing — no source-derived framing in comments, names, or structure
- [ ] leak — `./tools/leak-scan.sh` green, no capture cited by path instead of ID

## Notes

<!-- What was checked and what was found. An escalation (a behavior with no citable
     permitted source) is recorded here and blocks the change. -->
