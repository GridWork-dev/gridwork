subject: <paste the digest from `./tools/cleanroom-gate.sh --subject`>
reviewer: <the reviewing lane — a fresh-context session with no exposure to the implementing session>
date: <YYYY-MM-DD>

# Second-reader record

## Scope

<!-- The gated paths reviewed, and what the change does to them, in a sentence. -->

## Checks

- [ ] citations — every non-obvious terminal behavior carries a `Derivation:` marker
      naming a permitted source, and the marker sits on the behavior it actually
      describes. `cleanroom-gate` already proved every cited ID exists in
      `docs/derivation/SPECS.md` or `docs/derivation/CAPTURES.md`; what it cannot check,
      and you can, is whether the citation is TRUE — that the code does what that source
      says, and came from it
- [ ] framing — no source-derived framing in comments, names, or structure
- [ ] references — no naming of other projects as comparands, and no capture cited by
      path: captures are referenced by their registered ID and SHA-256, never otherwise
- [ ] leak — `./tools/leak-scan.sh` green

## Notes

<!-- What was checked and what was found. An escalation (a behavior with no citable
     permitted source) is recorded here and blocks the change. -->
