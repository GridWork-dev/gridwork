//! The local engine-parity harness — `docs/PARITY.md`'s own design contract,
//! turned into runnable code.
//!
//! Two halves, deliberately separated so public CI can gate one of them:
//!
//! - [`matrix`], [`pins`], and [`checks`] are the PURE library: a data model
//!   for the four-axis, three-engine matrix, the version pins and the `D`
//!   bound `docs/PARITY.md` defines, and one check function per axis. Every
//!   check is proven able to fail (a seeded negative) as well as pass (a
//!   positive control) — `docs/PARITY.md`: "a harness axis that cannot fail
//!   is a defect." This half has no process, no network, and no `#[ignore]`
//!   — public CI runs it on every push.
//! - [`runners`] drives the three engines for real: spawns the pinned CLI
//!   through its adapter's own public control-channel entry point, collects
//!   whatever the adapter normalizes, and hands the result to the matching
//!   pure check. These need a logged-in engine CLI on `PATH` and are only
//!   ever reached from `#[ignore]`d tests (`tests/*.rs`) or the local driver
//!   binary (`src/bin/parity.rs`) — never from `cargo test` without
//!   `--ignored`, and never from public CI at all.
//!
//! # The one rule this crate is bound to — and how the crate splits under it
//!
//! [`runners`] and `src/bin` (the harness entry point, plus the Claude
//! approval-relay hook it spawns as a genuinely separate process) ARE
//! listed in `.github/cleanroom-paths.txt` — engine supervision by the
//! category rule's own words: spawn the pinned CLI, drive it, answer its
//! approval asks, kill it. An edit under either prefix owes a
//! `Derivation:` marker plus a second-reader record at
//! `docs/derivation/reviews/`. [`matrix`], [`pins`], [`checks`], and this
//! file carry the not-engine line in `.github/cleanroom-not-engine.txt`
//! instead and owe neither: that half has no process, no network, and no
//! `#[ignore]` — it only ever sees the already-normalized facts the
//! runners hand it, never a live engine or a wire byte.
//!
//! The rule that bounds what the gated half owes is one hard rule
//! `docs/PARITY.md` states for the harness specifically: it drives engines
//! and checks results EXCLUSIVELY through the adapters' public APIs, and it
//! never hand-encodes a raw protocol wire frame or re-derives protocol
//! behavior the adapters are supposed to own. Every runner's module doc
//! says exactly which adapter functions it goes through, and — where a
//! runner's live interaction needed a request shape no adapter function
//! builds (starting a codex thread, creating an opencode session, Claude's
//! stream-json turn and its `PreToolUse` hook relay) — cites the same
//! canonical, already-adapter-cited source the schemas it uses were
//! vendored from: `CODEX-APP-SERVER`, `OPENCODE-SERVER`,
//! `CLAUDE-STREAM-JSON`, and `CLAUDE-HOOKS` in `docs/derivation/SPECS.md`,
//! never a guess.

pub mod checks;
pub mod matrix;
pub mod pins;

pub mod runners;
