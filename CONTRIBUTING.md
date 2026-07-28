# Contributing

Pre-1.0: the fastest way to help is issues with concrete reproductions, and small
focused PRs. Large features will churn under you until the contract stabilizes — open
an issue first.

## Licensing

Inbound = outbound: by submitting a contribution you license it under
[Apache-2.0](LICENSE), the project license. There is no CLA and no DCO sign-off.

## The one hard content rule

This project independently implements terminal multiplexing and agent orchestration.
**Do not submit code copied, ported, or mechanically translated from AGPL-licensed (or
any incompatibly-licensed) projects.** Concept-level inspiration is fine; derived code
is not, and terminal-engine-adjacent changes get an additional independent review for
exactly this reason. The full procedure — who may read what, derivation citations, the
second review — is public policy in [CLEANROOM.md](CLEANROOM.md).

## AI-assisted contributions

Most of this codebase is agent-authored under human direction; AI-assisted PRs are
welcome and held to the identical bar. Disclose heavy agent involvement in the PR
description, review your own diff before submitting, and never submit code you cannot
explain.

## Quality gate

Every PR must be green on:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo test --workspace --doc --locked
cargo deny check
```

CI also enforces MSRV (1.88) and a leak scan. Commit style is
[Conventional Commits](https://www.conventionalcommits.org) (`feat(scope):`,
`fix(scope):`, `chore(scope):`).

## Code floor (the short version)

- Typed errors in library crates; context catch-alls only at binary boundaries.
- No `unwrap()` in non-test code (CI-denied); `expect()` only with a message proving
  the invariant.
- `unsafe` is workspace-forbidden; the single future FFI adapter crate is the only
  sanctioned exception.
- Bounded channels everywhere; no blocking work on async executor threads.
- Newtypes for identifiers; `#[serde(deny_unknown_fields)]` at external boundaries;
  raw terminal bytes stay `[u8]` end to end.
