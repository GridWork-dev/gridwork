# Contributing

Pre-1.0: the fastest way to help is issues with concrete reproductions, and small
focused PRs. Large features will churn under you until the contract stabilizes — open
an issue first.

## What's actually useful right now

The kernel is being built (stage 2 of the [roadmap](ROADMAP.md)) and none of it has
merged, so "implement a feature" mostly isn't a thing you can do yet. What is:

- **Read the contract and argue with it.** `crates/gwk-domain/src/fsm.rs` and
  `transition.rs` hold the entire state-machine contract — four enums, four edge
  tables, and the one transition function every writer goes through. A missing edge, an
  unreachable state, or a guard that lets something through is the highest-value bug in
  the repo right now, and it is cheap to check — the tables are data.
- **Implement the storage port against a backend that isn't PostgreSQL.**
  `gwk_domain::port::EventStore` is four async methods, and `gwk_cert::conformance`
  gives you eight checks to run against your implementation. The in-memory store in
  that module is the reference. This is the most valuable surface in `gwk-cert` and
  currently the least visible.
- **Consume the generated TypeScript** in `contracts/bindings.ts` and tell us where it
  is wrong or unusable.
- Documentation gaps, unclear errors, and anything in the repo that reads as shipped
  when it isn't.

## Getting set up

A stable toolchain covers most of it. The full gate reaches past Rust, because four
other things get checked:

| For | Install |
|---|---|
| the MSRV job | `rustup toolchain install 1.94` — the floor is MSRV (1.94) |
| `cargo deny` | `cargo install cargo-deny` |
| the generated TypeScript | [Bun](https://bun.sh) — CI pins **1.3.14** |
| the SQL DDL | a PostgreSQL 16 you can point `psql` at |
| the site image | Docker |

**You do not need any of them to contribute** — CI installs its own copy of each. Reach
for a row only when you want to reproduce that job locally. One exception worth
knowing: touching `gwk-domain` or `gwk-theme` pulls in Bun whether you like it or not,
because the generated contract has to be regenerated and committed (below).

There is no `rust-toolchain.toml` and no one-command CI equivalent.

## The local gate

Rust:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo test --workspace --doc --locked
cargo deny check
```

The generated contract, if you touched `gwk-domain` or `gwk-theme`:

```bash
cargo run --locked -p xtask -- contract --check
cd contracts && bun install --frozen-lockfile && bun test && bun x tsc --noEmit && cd -
./tools/check-theme-sync.sh
```

Publication gates, on every PR:

```bash
./tools/leak-scan.sh
./tools/check-claims.sh
./tools/leak-scan.sh --history main..HEAD   # what CI's commit-messages job covers
```

The three jobs left — `msrv`, `schema`, `site` — need the extra toolchains, and CI is
usually the cheaper place to run them. Locally they are:

```bash
cargo +1.94 check --workspace --all-targets --locked
psql "$PGURL" -v ON_ERROR_STOP=1 -f schema/0001_contract.sql
docker build -f site/Dockerfile .
```

**`cargo deny check` is stricter locally than the merge gate.** CI splits it: bans,
licenses, and sources block a merge; `advisories` runs as its own non-blocking job, so
a newly published advisory can't redden an unchanged PR. If `advisories` is the only
red X on your PR, it is not your PR.

## Regenerating the contract

`gwk-domain` and `gwk-theme` are the source of truth; the TypeScript, the goldens, and
the theme JSON are generated from them. Edit a type and you must regenerate:

```bash
cargo run -p xtask -- contract     # writes contracts/bindings.ts, goldens/, signal-theme.json
```

Commit what it writes. Two things that will confuse you the first time:

- **`bun test` rewrites `contracts/goldens-ts/` as a side effect** — that is the test,
  not a mess. CI runs `git diff --exit-code contracts/goldens-ts` right after, so a
  diff there means the TypeScript decode path changed what it read. Commit it or
  explain it; don't reflexively `git checkout` it away.
- The SQL DDL is checked against the Rust edge tables by `crates/gwk-domain/tests/ddl_parity.rs`.
  Change an edge and `schema/0001_contract.sql` changes with it, in the same commit.

## Two gates that will surprise you

**`tools/check-claims.sh`** pins claims that appear in more than one file — MSRV, the
install command, the current stage number, the binary name, the terminal-only stance.
Copy drifts one file at a time, so editing the stage number in `README.md` alone goes
red until the site, `ROADMAP.md`, and the threat model agree. When it fails it tells you
which file and which pattern.

**`tools/leak-scan.sh`** keeps private-estate identifiers out of public history. Three
rules that catch people:

- **No binary files.** Every tracked file must be greppable text. There is exactly one
  reviewed exemption (`site/og.png`), pinned to that path and to `image/png`.
- **No absolute home paths** — the `/home/<user>/` and `/Users/<user>/` shapes — and no
  credential assignments (`_TOKEN`, `_SECRET`, `_API_KEY`, `_PASSWORD` immediately
  followed by `=` and a value), private-key blocks, CGNAT addresses, or agent-session
  URLs. Operator machines load extra estate-specific patterns from an untracked
  `tools/leak-scan.local`, so a clean local run is necessary, not sufficient.
- **CI scans commit messages, the range diff, and every individual commit patch** — not
  just the final tree. That happens in the separate `commit-messages` job; the bare
  `./tools/leak-scan.sh` only reads the tracked tree, so use
  `./tools/leak-scan.sh --history main..HEAD` to cover the same ground before you push.
  Adding a bad value and removing it in a later commit does *not* clear the gate — the
  fix is to rebase it out of history.

The leak gate proves it can fail: CI feeds it three seeded violations — a home path, an
agent-session URL, and a leak that exists only in an intermediate commit — and fails the
build if the scan accepts any of them. `check-claims.sh` has no such self-test.

## Licensing and provenance

Inbound = outbound: by submitting a contribution you license it under
[Apache-2.0](LICENSE), the project license, and you attest you have the right to
contribute the work under it. There is no CLA and no DCO sign-off.

## The one hard content rule

This project independently implements terminal multiplexing and agent orchestration.
**Do not submit code copied, ported, or mechanically translated from AGPL-licensed (or
any incompatibly-licensed) projects.** Concept-level inspiration is fine; derived code
is not, and terminal-engine-adjacent changes get an additional independent review for
exactly this reason. The full procedure — who may read what, derivation citations, the
second review — is public policy in [CLEANROOM.md](CLEANROOM.md). The paths it covers
are listed in `.github/cleanroom-paths.txt`.

## AI-assisted contributions

Most of this codebase is agent-authored under human direction; AI-assisted PRs are
welcome and held to the identical bar. Disclose heavy agent involvement in the PR
description, review your own diff before submitting, and never submit code you cannot
explain.

AI assistance is disclosed with a non-authorship trailer — `AI-Assisted-By: <tool>` — on
the commits it applies to. AI tools are never listed as authors or co-authors.

## Commits

[Conventional Commits](https://www.conventionalcommits.org): `feat(scope):`,
`fix(scope):`, `chore(scope):`, `docs(scope):`. One logical change per commit.

## Code floor (the short version)

- Typed errors in library crates; context catch-alls only at binary boundaries.
- No `unwrap()` in non-test code (CI-denied); `expect()` only with a message proving
  the invariant.
- `unsafe` is workspace-forbidden; the single future FFI adapter crate is the only
  sanctioned exception.
- Bounded channels everywhere; no blocking work on async executor threads.
- Newtypes for identifiers; `#[serde(deny_unknown_fields)]` at external boundaries;
  raw terminal bytes stay `[u8]` end to end.

## Filing an issue

Use a template — bug reports and contract feedback have different shapes and the
templates ask for what actually helps. Anything exploitable goes through
[private vulnerability reporting](https://github.com/GridWork-dev/gridwork/security/advisories/new)
instead; see [SECURITY.md](SECURITY.md).
