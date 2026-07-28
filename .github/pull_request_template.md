## What

<!-- What this changes and why, one paragraph. -->

## Checks

- [ ] Rust gate green locally (the commands under "The local gate" in CONTRIBUTING.md)
- [ ] Publication gates green locally (`./tools/leak-scan.sh`, `./tools/check-claims.sh`)
- [ ] Touched `gwk-domain`/`gwk-theme`? Contract regenerated and committed — `cargo run -p xtask -- contract` (bindings.ts, goldens/, signal-theme.json) **and** `cd contracts && bun test` (rewrites `goldens-ts/`; commit it)
- [ ] Heavy AI assistance disclosed (see CONTRIBUTING.md)
- [ ] Contains no code derived from AGPL/GPL or otherwise incompatibly-licensed projects
- [ ] If this touches clean-room paths (`.github/cleanroom-paths.txt`): derivation citations included (CLEANROOM.md)

<!--
Eight checks gate the merge: test, msrv, schema, contract, site, deny, leak-scan,
commit-messages. `advisories` also runs and is NOT one of them — if it is your only
red X, it is not your PR.

`commit-messages` runs the leak scanner over commit messages, the range diff, and
every individual commit patch — not just the final tree. Adding a bad value and
removing it later does not clear it; rebase.
-->
