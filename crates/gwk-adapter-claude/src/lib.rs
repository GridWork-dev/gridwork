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
//! The normalization surface all three adapters converge on is
//! [`gwk_domain::engine::EngineAdapter`], which [`adapter`] implements over
//! this crate's own normalized event type, so the kernel side sees the same
//! shape the real ACP adapter presents.
//!
//! It is deliberately NOT the ACP SDK's `Agent`. This doc said so for three
//! phases and could not be made true: `Agent` is a zero-sized marker struct
//! tagging one end of a JSON-RPC connection, not a trait, so nothing can
//! implement it — and standing up the connection it tags would mean running an
//! ACP endpoint here to speak to ourselves. This crate carries no ACP SDK
//! dependency, and that is the correct number for an engine that does not speak
//! that wire.
//!
//! # Clean-room scope
//!
//! This crate is under `CLEANROOM.md`'s second-review gate
//! (`.github/cleanroom-paths.txt`). Every non-obvious protocol behavior
//! carries a `Derivation:` marker citing a row in `docs/derivation/SPECS.md`
//! — `CLAUDE-STREAM-JSON`, `CLAUDE-HEADLESS`, `CLAUDE-HOOKS`, or
//! `CLAUDE-AGENT-SDK`, each scoped to exactly what its named page states.
//! The fourth row resolved a round of escalations this crate first shipped
//! honestly unresolved: `result.usage`'s key names, `duration_ms`,
//! `num_turns`, `result.subtype`'s exact string values, and the "estimate"
//! characterization of `total_cost_usd` are now cited against
//! `code.claude.com/docs/en/agent-sdk/typescript`'s own published
//! `SDKResultMessage`/`Usage` types — the typed surface over the same wire
//! messages, not a separate protocol. One escalation survives: a
//! `tool_use` content block's own JSON shape, which that page explicitly
//! delegates to `MessageParam`, "From Anthropic SDK" — a third, uncited
//! surface. See [`message`] and [`cost`] for exactly where each citation
//! and each remaining escalation sits.

pub mod adapter;
pub mod cost;
pub mod hook;
pub mod message;
pub mod relay;
pub mod stream;

mod io_util;

pub use adapter::{ClaudeAdapter, ClaudeSignal};

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
// Derivation: CAP-003 — with `--print`, `--output-format stream-json` is
// refused unless `--verbose` is present ("Error: When using --print,
// --output-format=stream-json requires --verbose", pinned CLI 2.1.220,
// exit 1).
pub fn control_command() -> std::process::Command {
    let mut command = std::process::Command::new(ENGINE);
    command.args([
        "--print",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
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
                "stream-json",
                "--verbose"
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
