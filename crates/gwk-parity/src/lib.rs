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
//! # The one rule this crate is bound to
//!
//! This crate is NOT under `.github/cleanroom-paths.txt` — it carries no
//! `Derivation:` markers and needs no second reader. What keeps it that way
//! is one hard rule `docs/PARITY.md` states for the harness specifically:
//! it drives engines and checks results EXCLUSIVELY through the adapters'
//! public APIs, and it never hand-encodes a raw protocol wire frame or
//! re-derives protocol behavior the adapters are supposed to own. Every
//! runner's module doc says exactly which adapter functions it goes
//! through, and — where a runner's live interaction needed a request shape
//! no adapter function builds (starting a codex thread, creating an
//! opencode session) — cites the same canonical, already-adapter-cited
//! generation command the schemas it uses were vendored from, never a
//! guess.

pub mod checks;
pub mod matrix;
pub mod pins;

pub mod runners;
