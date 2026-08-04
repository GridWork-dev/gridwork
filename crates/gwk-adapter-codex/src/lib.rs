//! The codex adapter: a vendor protocol, not ACP.
//!
//! Control and rendering are separate channels by design. The engine's native
//! TUI is spawned under a [`gwk_pty::Session`] and only ever *rendered*;
//! control travels the engine's app-server JSON-RPC interface, whose
//! engine-generated schemas will be vendored beside this crate with a
//! freshness check — a vendored schema nobody re-generates is a comment
//! wearing a pin's name. Control never rides synthetic keystrokes.
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

use gwk_domain::EngineId;
use gwk_pty::{Session, SpawnError};

pub mod adapter;
pub mod command;
pub mod event;
pub mod schema;
pub mod wire;

pub use adapter::CodexAdapter;
pub use command::{
    DecisionMappingError, command_execution_response, file_change_response,
    open_gate_for_command_execution, open_gate_for_file_change, record_cost_entry,
};
pub use event::{
    ApprovalRequest, CodexEvent, NormalizeError, normalize_notification, normalize_request,
};
pub use wire::{Frame, JsonRpcId, WireClient, WireError};

/// The engine CLI this adapter drives.
pub const ENGINE: &str = "codex";

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

/// The control half's invocation.
// Derivation: CODEX-APP-SERVER — `codex app-server` speaks bidirectional
// JSON-RPC 2.0 as newline-delimited JSON over stdio by default.
pub fn control_command() -> std::process::Command {
    let mut command = std::process::Command::new(ENGINE);
    command.arg("app-server");
    command
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_control_channel_is_the_app_server() {
        let command = control_command();
        assert_eq!(command.get_program(), "codex");
        let args: Vec<&std::ffi::OsStr> = command.get_args().collect();
        assert_eq!(args, ["app-server"]);
        // No environment overrides: the engine's own login is the engine's own.
        assert_eq!(command.get_envs().count(), 0);
    }

    #[test]
    fn the_engine_identity_is_the_cli_name() {
        assert_eq!(engine_id().as_str(), ENGINE);
    }
}
