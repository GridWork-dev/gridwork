subject: <paste the digest from `./tools/cleanroom-gate.sh --subject`>
reviewer: <the reviewing lane — a fresh-context session with no exposure to the implementing session>
session: <the reviewing session's opaque id — an id only, never a path>
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
      path: captures are referenced by their registered ID and SHA-256, never otherwise.
      Record the finding as the CLASS searched and the HIT COUNT — "terminal-tooling
      comparand class (5 terms): 0 hits" — never the query terms themselves. A record
      that lists them publishes the exact strings this check exists to keep out, and
      the record is the one artifact nobody re-reviews. Give the term count only if you
      ran the grep and know it — never reconstruct one. A name already registered in
      ../SPECS.md is a permitted source, not a query term: cite it, and say whether you
      checked the protocol or the implementation
- [ ] leak — `./tools/leak-scan.sh` green

## Notes

<!-- What was checked and what was found. An escalation (a behavior with no citable
     permitted source) is recorded here and blocks the change. -->
