//! The claude-code adapter: a vendor protocol, not ACP.
//!
//! Control and rendering are separate channels by design. The engine's native
//! TUI is spawned under a [`gwk_pty::Session`] and only ever *rendered*;
//! control travels the engine's bidirectional stream-json interface
//! ([`stream::StreamClient`]), and the approval channel is the engine's own
//! `PreToolUse` hook — the one decision-returning path that exists when the
//! engine draws its own TUI ([`hook`], relayed by [`relay`]). Control never
//! rides synthetic keystrokes; nothing in [`stream`] or [`relay`] ever
//! reaches [`spawn_tui`].
//!
//! The normalization surface all three adapters converge on is the ACP SDK's
//! role machinery: this crate implements the `Agent` role server-side over
//! the vendor protocol, so the kernel side sees the same shape the real ACP
//! adapter presents.
//!
//! # Clean-room scope
//!
//! This crate is under `CLEANROOM.md`'s second-review gate
//! (`.github/cleanroom-paths.txt`). Every non-obvious protocol behavior
//! carries a `Derivation:` marker citing a row in `docs/derivation/SPECS.md`
//! — `CLAUDE-STREAM-JSON`, `CLAUDE-HEADLESS`, or `CLAUDE-HOOKS`, each scoped
//! to exactly what its named page states. Several fields this crate's own
//! design contract (`docs/PARITY.md`) names — `result.usage`'s key names,
//! `duration_ms`, `num_turns`, `result.subtype`'s exact string values, and
//! the "list-rate estimate" characterization of `total_cost_usd` — are not
//! stated on any of the three permitted pages; those are escalations
//! (CLEANROOM.md rule 3: "a behavior with no citable permitted source is an
//! escalation, not a guess"), not citations, and are called out at their
//! use sites in [`message`] and [`cost`] plus the dispatch report.

pub mod cost;
pub mod hook;
pub mod message;
pub mod relay;
pub mod stream;

mod io_util;

use gwk_domain::EngineId;
use gwk_pty::{Session, SpawnError};

/// The engine CLI this adapter drives.
pub const ENGINE: &str = "claude";

/// The identity this adapter reports into the contract — the `engine` value
/// on engine sessions and spend-ledger rows.
pub fn engine_id() -> EngineId {
    EngineId::new(ENGINE)
}

/// The render half: the engine's own interactive TUI under a PTY.
///
/// Nothing is asserted about what it draws — the PTY layer owns structural
/// truth — and nothing is ever typed into it by this adapter.
pub fn spawn_tui(cols: u16, rows: u16) -> Result<Session, SpawnError> {
    Session::spawn(pty_process::Command::new(ENGINE), cols, rows)
}

/// The control half's invocation: the headless bidirectional stream.
// Derivation: CLAUDE-STREAM-JSON — print mode accepts `--input-format
// stream-json` (stdin) and `--output-format stream-json` (stdout).
pub fn control_command() -> std::process::Command {
    let mut command = std::process::Command::new(ENGINE);
    command.args([
        "--print",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
    ]);
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_control_channel_is_the_bidirectional_stream() {
        let command = control_command();
        assert_eq!(command.get_program(), "claude");
        let args: Vec<&std::ffi::OsStr> = command.get_args().collect();
        assert_eq!(
            args,
            [
                "--print",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json"
            ]
        );
        // No environment scrubbing: the rendering spike recorded that the
        // env-scrub is not load-bearing, and the engine's own login is the
        // engine's own.
        assert_eq!(command.get_envs().count(), 0);
    }

    #[test]
    fn the_engine_identity_is_the_cli_name() {
        assert_eq!(engine_id().as_str(), ENGINE);
    }
}
