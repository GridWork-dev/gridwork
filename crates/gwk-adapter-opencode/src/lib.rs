//! The opencode adapter: the one true ACP wire.
//!
//! Control and rendering are separate channels by design. The engine's native
//! TUI is spawned under a [`gwk_pty::Session`] and only ever *rendered*;
//! everything the kernel decides — lifecycle, status, transcripts, approvals —
//! travels the engine's own control surface. Control never rides synthetic
//! keystrokes.
//!
//! The normalization surface all three adapters converge on is the ACP SDK's
//! role machinery (the `Agent` role behind a connection), which this crate
//! reaches over the real wire; the sibling adapters implement the same role
//! over their own vendor protocols.
//!
//! Lifecycle, status, and approval do not ride that ACP connection at all —
//! per `docs/PARITY.md`, they ride the engine's own server event bus
//! (`GET /event`) and REST surface instead. [`event`] normalizes that bus
//! into gwk's own event enum and the kernel-side commands it warrants;
//! [`cost`] turns per-child session usage into the spend ledger's command.
//! Neither module opens a connection or decides an approval — both produce
//! typed values only, for whatever host component owns the opencode HTTP
//! client to act on.

use agent_client_protocol::AcpAgentConfig;
use gwk_domain::EngineId;
use gwk_pty::{Session, SpawnError};

pub mod cost;
pub mod event;

/// The engine CLI this adapter drives.
pub const ENGINE: &str = "opencode";

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

/// The control half's subprocess configuration.
// Derivation: OPENCODE-ACP — `opencode acp` runs the engine as an
// ACP-compatible subprocess communicating over JSON-RPC via stdio.
pub fn acp_agent_config() -> AcpAgentConfig {
    AcpAgentConfig::new(ENGINE).arg("acp")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_control_channel_is_the_acp_subcommand() {
        let config = acp_agent_config();
        assert_eq!(config.command(), std::path::Path::new("opencode"));
        assert_eq!(config.arguments(), ["acp"]);
        // No environment overrides: the engine's own login is the engine's own.
        assert!(config.environment().is_empty());
    }

    #[test]
    fn the_engine_identity_is_the_cli_name() {
        assert_eq!(engine_id().as_str(), ENGINE);
    }
}
