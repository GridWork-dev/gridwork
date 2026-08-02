//! The live runners: one per engine per axis, twelve in all. Each is
//! `async` and takes no arguments beyond what it hard-codes (the CLI, the
//! pin, a bounded scripted prompt) — the shape `tests/*.rs`'s `#[ignore]`d
//! wrappers and [`run_all`] (the local driver, `src/bin/parity.rs`) both
//! call unchanged.
//!
//! # What every runner does, in order
//!
//! 1. Assert the installed CLI's `--version` output contains the pin from
//!    [`crate::pins`] — a missing binary or an unpinned version is a
//!    [`crate::matrix::Verdict::Skipped`] cell, never a hang and never a
//!    false pass (`docs/PARITY.md`'s own harness rule).
//! 2. Spawn the engine through its adapter's public control-channel entry
//!    point (never `spawn_tui` — that renders, it does not control).
//! 3. Drive a minimal, non-interactive scripted interaction.
//! 4. Collect whatever the adapter normalizes, under [`RUNNER_TIMEOUT`].
//! 5. Hand the collected, normalized facts to the matching pure function in
//!    [`crate::checks`].
//! 6. Kill the child on every path out — success, failure, or timeout.
//!
//! # Where a runner could not reach a real engine-triggered fact
//!
//! Two adapters (`gwk-adapter-codex`, `gwk-adapter-opencode`) expose
//! response-shapes for facts the ENGINE sends unprompted, but no function
//! for the requests a caller would send to START a thread or a session —
//! that surface is a future phase's, not this adapter's. Driving one live
//! required the exact client-request shape the adapter itself does not
//! (yet) type. Rather than guess, the codex runners cite the same canonical
//! source `gwk-adapter-codex/schemas/PROVENANCE.md` already vendors from —
//! `codex app-server generate-json-schema`, run against the PINNED 0.146.0
//! binary — for the two request shapes (`thread/start`/`turn/start`) it
//! needed and the adapter does not vendor; the opencode runners cite the
//! same `GET /doc` OpenAPI source `gwk-adapter-opencode`'s own module docs
//! already read their citations from. Neither is an invention; both are the
//! identical generation command/endpoint the adapter crates' own doc
//! comments name, read one layer further than what got vendored. Where even
//! that source did not settle a shape (opencode's default permission
//! ruleset for a scripted session), the runner says so and records
//! [`crate::matrix::Verdict::Skipped`] rather than guess.

use std::time::Duration;

use tokio::process::Command;

use crate::matrix::{Axis, Cell, Engine};

pub mod claude;
pub mod codex;
pub mod opencode;

/// Hard per-runner ceiling. Nothing below may block past it — every
/// `next_message`/`recv`/socket-accept call is wrapped in
/// [`tokio::time::timeout`] with this budget (or a fraction of it for a
/// runner with more than one network round trip).
pub const RUNNER_TIMEOUT: Duration = Duration::from_secs(45);

/// Whether `<binary> --version`'s output contains `pin`.
pub(crate) enum VersionCheck {
    /// The binary is not on `PATH` at all.
    Absent,
    /// It ran, but reported a different version.
    Mismatch(String),
}

pub(crate) async fn check_version(binary: &str, pin: &str) -> Result<(), VersionCheck> {
    let output = match Command::new(binary).arg("--version").output().await {
        Ok(o) => o,
        Err(_) => return Err(VersionCheck::Absent),
    };
    let text = String::from_utf8_lossy(&output.stdout);
    if text.contains(pin) {
        Ok(())
    } else {
        Err(VersionCheck::Mismatch(text.trim().to_owned()))
    }
}

/// The [`Cell`] every runner returns when its engine's version check comes
/// back [`VersionCheck::Absent`] or [`VersionCheck::Mismatch`].
pub(crate) fn version_skip(engine: Engine, axis: Axis, binary: &str, check: VersionCheck) -> Cell {
    match check {
        VersionCheck::Absent => Cell::skipped(
            engine,
            axis,
            format!(
                "`{binary}` is not on PATH — skipping (a missing engine is a skip, \
                 never a hang or a failure, per docs/PARITY.md)"
            ),
        ),
        VersionCheck::Mismatch(actual) => Cell::skipped(
            engine,
            axis,
            format!(
                "`{binary} --version` reported {actual:?}, not the pinned version — \
                 skipping rather than judging an unpinned build"
            ),
        ),
    }
}

/// Run all twelve live runners and fill a [`crate::matrix::Matrix`] — the
/// one place that lists every axis for every engine, shared by
/// `src/bin/parity.rs` and (indirectly, one axis at a time) `tests/*.rs`.
pub async fn run_all() -> crate::matrix::Matrix {
    let mut matrix = crate::matrix::Matrix::new();
    matrix.record(claude::lifecycle().await);
    matrix.record(claude::status_truth().await);
    matrix.record(claude::transcript_ingestion().await);
    matrix.record(claude::approval_relay().await);
    matrix.record(codex::lifecycle().await);
    matrix.record(codex::status_truth().await);
    matrix.record(codex::transcript_ingestion().await);
    matrix.record(codex::approval_relay().await);
    matrix.record(opencode::lifecycle().await);
    matrix.record(opencode::status_truth().await);
    matrix.record(opencode::transcript_ingestion().await);
    matrix.record(opencode::approval_relay().await);
    matrix
}
