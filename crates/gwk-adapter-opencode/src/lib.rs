//! The opencode adapter: the one true ACP wire.
//!
//! Control and rendering are separate channels by design. The engine's native
//! TUI is spawned under a [`gwk_pty::Session`] and only ever *rendered*;
//! everything the kernel decides — lifecycle, status, transcripts, approvals —
//! travels the engine's own control surface. Control never rides synthetic
//! keystrokes.
//!
//! The normalization surface all three adapters converge on is
//! [`gwk_domain::engine::EngineAdapter`], which [`adapter`] implements over
//! this crate's own normalized event type; the sibling adapters implement the
//! same trait over their vendor protocols.
//!
//! This is the crate that speaks a real ACP wire, and it is worth being precise
//! about why that does not make ACP the normalization layer. ACP drives this
//! engine's agent loop; lifecycle, status and approval do not ride that
//! connection at all — they ride the server's own event bus, as the next
//! paragraph says. So even here the convergence is on gwk's types, and the
//! sibling adapters are not approximating something this one gets natively.
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

pub mod adapter;
pub mod cost;
pub mod event;

pub use adapter::OpencodeAdapter;

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
///
/// And nothing is asserted about what the engine loads: a pane's working
/// directory does not select the engine's plugin set, and no artifact in
/// this workspace may claim it does.
// Derivation: OPENCODE-PLUGINS — the page fixes the plugin homes
// (`.opencode/plugins/` project-level, `~/.config/opencode/plugins/`
// global, npm packages from config) and hands a plugin `directory` ("the
// current working directory") and `worktree` ("the git worktree path") as
// DISTINCT context values, while stating no rule that connects the working
// directory to which plugins load. cwd-equals-project is therefore an
// inference the document does not license, and this crate does not make it.
pub fn spawn_tui(cols: u16, rows: u16) -> Result<Session, SpawnError> {
    Session::spawn(pty_process::Command::new(ENGINE), cols, rows)
}

/// The control half's subprocess configuration.
///
/// A pane hosting this mode inherits NONE of the mux's persistence, detach,
/// or multi-client guarantees. Those guarantees are properties of the PTY
/// session path (`spawn_tui` under the host's session registry); this is a
/// JSON-RPC subprocess whose life is its stdio connection, and the document
/// that licenses it grants nothing that would extend the mux's guarantees
/// over it — so nothing here may imply the surrounding mux covers it.
// Derivation: OPENCODE-ACP — `opencode acp` runs the engine as an
// ACP-compatible subprocess communicating over JSON-RPC via stdio. The
// page's parity claim ("works the same via ACP as it does in the terminal.
// All features are supported") carries its own named, time-qualified
// exception ("some built-in slash commands like `/undo` and `/redo` are
// currently unsupported") — and it is a claim about features, on a page
// that says nothing about session persistence, detach or reattach,
// multiple simultaneous clients, or stderr. That such a silence grants no
// guarantee — that a pane hosting this mode must claim none of the four —
// is this crate's reading of the document's scope, not a sentence it
// contains.
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
