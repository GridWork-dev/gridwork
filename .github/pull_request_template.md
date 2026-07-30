## What

<!-- What this changes and why, one paragraph. -->

## Checks

- [ ] Rust gate green locally (the commands under "The local gate" in CONTRIBUTING.md)
- [ ] Publication gates green locally (`./tools/leak-scan.sh`, `./tools/check-claims.sh`)
- [ ] Touched `gwk-domain`/`gwk-theme`? Contract regenerated and committed — `cargo run -p xtask -- contract` (bindings.ts, goldens/, signal-theme.json) **and** `cd contracts && bun test` (rewrites `goldens-ts/`; commit it)
- [ ] Heavy AI assistance disclosed (see CONTRIBUTING.md)
- [ ] Contains no code derived from AGPL/GPL or otherwise incompatibly-licensed projects
- [ ] If this touches clean-room paths (`.github/cleanroom-paths.txt`): derivation citations included, and a second-reader record committed under `docs/derivation/reviews/` (CLEANROOM.md rule 4 — `./tools/cleanroom-gate.sh --subject` names the file)

<!--
Twelve checks gate the merge: test, msrv, schema, contract, site, deny, leak-scan,
commit-messages, kernel-integration, package, macos, cleanroom-gate. `perf` and
`advisories` also run and are NOT among them — if one of those is your only red X,
it is not your PR.

`commit-messages` runs the leak scanner over commit messages, the range diff, and
every individual commit patch — not just the final tree. Adding a bad value and
removing it later does not clear it; rebase.
-->
