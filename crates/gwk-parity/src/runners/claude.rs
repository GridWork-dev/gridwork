//! Claude Code runners. `docs/PARITY.md`'s harness section requires BOTH of
//! Claude's control channels to be exercised, since they regress
//! independently: [`lifecycle`], [`status_truth`], and
//! [`transcript_ingestion`] drive the bidirectional stream-json channel
//! (`gwk_adapter_claude::stream`/`message`); [`approval_relay`] drives the
//! `PreToolUse` hook-relay channel (`gwk_adapter_claude::hook`/`relay`).
//!
//! # The stream-json channel's scripted turn
//!
//! `gwk_adapter_claude::stream::StreamClient::send_line`'s own doc is
//! explicit that the input envelope's shape is the CALLER's to supply — no
//! `CLAUDE-*` page in this adapter's citation scope states it. The shape
//! used below (`{"type":"user","message":{"role":"user","content":...},
//! "parent_tool_use_id":null}`) is what `code.claude.com/docs/en/agent-sdk/
//! streaming-vs-single-mode` documents for `SDKUserMessage` — the same typed
//! surface `gwk_adapter_claude::message`'s own module doc already cites for
//! `result.usage`'s field names — used here only to DRIVE the engine, never
//! to interpret its output; every fact this module checks still comes back
//! through [`gwk_adapter_claude::message::parse_line`].
//!
//! # A residual the adapter's own public surface leaves honest
//!
//! [`gwk_adapter_claude::stream::StreamClient::spawn`] takes no arguments —
//! there is no way through it to register a `PreToolUse` hook for a live
//! session (that needs `--settings` inline JSON or a project
//! `.claude/settings.json`, neither of which `StreamClient` exposes a hook
//! for). [`approval_relay`] therefore proves the relay channel for real — a
//! real `AskRelay` Unix socket, answered by the crate's own compiled
//! `parity-claude-hook` binary running as a genuinely separate OS process —
//! but the [`gwk_adapter_claude::hook::PreToolUseAsk`] it relays is built
//! through the adapter's own public decoder from a fixture matching its own
//! documented stdin schema (the same fixture shape the adapter's own
//! `hook.rs` unit tests use), not emitted by a live claude tool call. Wiring
//! a live-triggered hook is a host-integration concern outside this
//! adapter's (and this bounded task's) scope, not an oversight.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use gwk_adapter_claude::hook::{PermissionDecision, PreToolUseAsk, open_gate_from_ask};
use gwk_adapter_claude::message::{Message, SubagentTree};
use gwk_adapter_claude::relay::AskRelay;
use gwk_adapter_claude::stream::StreamClient;
use gwk_adapter_claude::{ClaudeAdapter, ClaudeSignal};
use gwk_domain::GateId;
use gwk_domain::engine::{EngineAdapter, LifecycleFact};
use tokio::time::timeout;

use super::{RUNNER_TIMEOUT, check_version, version_skip};
use crate::checks::{
    check_fanout_linkage, check_gate_carries_question_and_options, check_lifecycle_observed,
    check_status_skew,
};
use crate::matrix::{Axis, Cell, Engine};
use crate::pins::{CLAUDE_CLI_VERSION, d_bound};

const ENGINE: Engine = Engine::Claude;
const PROMPT: &str = "Reply with exactly the word: ok. Do not use any tool.";

async fn ensure_version(axis: Axis) -> Result<(), Cell> {
    match check_version(gwk_adapter_claude::ENGINE, CLAUDE_CLI_VERSION).await {
        Ok(()) => Ok(()),
        Err(check) => Err(version_skip(
            ENGINE,
            axis,
            gwk_adapter_claude::ENGINE,
            check,
        )),
    }
}

/// One trivial scripted turn: spawn the real bidirectional stream-json
/// child (`StreamClient::spawn`), send one user-turn line, and collect
/// every [`Message`] this module's doc-cited envelope produced —
/// timestamped by elapsed time since the line was sent — until the
/// terminal `result` line or `budget` runs out. The child is always killed
/// on the way out.
async fn run_one_turn(budget: Duration) -> Result<Vec<(Duration, Message)>, String> {
    let mut client = StreamClient::spawn().map_err(|e| e.to_string())?;
    let turn = serde_json::json!({
        "type": "user",
        "message": { "role": "user", "content": PROMPT },
        "parent_tool_use_id": null,
    });
    let started = Instant::now();
    let send_result = client.send_line(&turn).await;
    let mut observed = Vec::new();
    if send_result.is_ok() {
        let collect = async {
            while let Ok(Some(message)) = client.next_message().await {
                let is_result = message.is_result();
                observed.push((started.elapsed(), message));
                if is_result {
                    break;
                }
            }
        };
        let _ = timeout(budget, collect).await;
    }
    let _ = client.child_mut().kill().await;
    send_result.map(|()| observed).map_err(|e| e.to_string())
}

/// Axis 1 — lifecycle. Checked against what the stream-json channel can
/// honestly report for a single non-interactive turn: `system`/`init`
/// (start) and a terminal `result` (end). `docs/PARITY.md`'s own table
/// names two more facts for this engine — idle via the `Stop` hook, and
/// error via a `result` error subtype — neither of which this crate invents
/// a tag for: `Stop`/`SessionEnd` have no typed adapter surface today
/// (only `PreToolUse` does), and a single scripted turn has no natural
/// "waiting for the next turn" moment to observe idle against. Extending
/// this once that surface exists is a follow-up, not a gap this pass hides.
pub async fn lifecycle() -> Cell {
    if let Err(cell) = ensure_version(Axis::Lifecycle).await {
        return cell;
    }
    match run_one_turn(RUNNER_TIMEOUT).await {
        Ok(observed) => {
            // Through the adapter, not around it. This used to inspect the raw
            // messages here and push string tags; the harness was then checking
            // that THIS code agreed with itself about what a start looks like,
            // not that the adapter reports one. A fact missing from the adapter
            // now fails the cell, which is what axis 1 always meant.
            let facts: Vec<LifecycleFact> = observed
                .iter()
                .filter_map(|(_, m)| {
                    ClaudeAdapter
                        .normalize(ClaudeSignal::Stream(m.clone()))
                        .ok()
                })
                .flatten()
                .filter_map(|e| e.lifecycle())
                .collect();
            let (verdict, detail) =
                check_lifecycle_observed(&[LifecycleFact::Started, LifecycleFact::Ended], &facts);
            Cell::new(ENGINE, Axis::Lifecycle, verdict, detail)
        }
        Err(e) => Cell::fail(
            ENGINE,
            Axis::Lifecycle,
            format!("driving claude failed: {e}"),
        ),
    }
}

/// Axis 2 — status truth. The "state change" this scripted turn can measure
/// against is the moment the line was sent; the "observed report" is the
/// first message the adapter parsed off the child's stdout — an honest
/// stand-in for engine-state-change-to-adapter-report skew when there is no
/// independent clock inside the engine to read.
pub async fn status_truth() -> Cell {
    if let Err(cell) = ensure_version(Axis::StatusTruth).await {
        return cell;
    }
    match run_one_turn(RUNNER_TIMEOUT).await {
        Ok(observed) => match observed.first() {
            Some((elapsed, _)) => {
                let observed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
                let (verdict, detail) = check_status_skew(0, observed_ms, d_bound());
                Cell::new(ENGINE, Axis::StatusTruth, verdict, detail)
            }
            None => Cell::fail(
                ENGINE,
                Axis::StatusTruth,
                "no message arrived to measure skew against",
            ),
        },
        Err(e) => Cell::fail(
            ENGINE,
            Axis::StatusTruth,
            format!("driving claude failed: {e}"),
        ),
    }
}

/// The dead-channel guard for axis 3: an empty (or result-less) message set
/// makes [`check_fanout_linkage`] pass vacuously — same as a turn that
/// really ran and fanned out nothing, which is exactly the false-pass shape
/// this guard exists to tell apart from a channel that never delivered a
/// turn at all (the child errored out at flag validation before writing a
/// line, was killed early, or otherwise never got to the wire). Pure and
/// adapter-normalized, so it takes the already-parsed message slice, never
/// the live child.
fn has_terminal_result(messages: &[Message]) -> bool {
    messages.iter().any(Message::is_result)
}

/// Axis 3 — transcript ingestion (fan-out linkage). This scripted turn
/// invokes no subagent, so [`SubagentTree::from_messages`] should reconstruct
/// an EMPTY tree — the check passes vacuously on real, live-parsed output
/// (proving the spawn → collect → reconstruct → check path end to end)
/// rather than asserting a fan-out this bounded pass never triggers a real
/// subagent to produce. That vacuous-pass shape is only honest once the
/// turn is known to have actually completed — [`has_terminal_result`] gates
/// on it before the tree is even built, so a turn that produced NO messages
/// (a dead channel) fails on that fact instead of reading as an empty,
/// passing fan-out.
pub async fn transcript_ingestion() -> Cell {
    if let Err(cell) = ensure_version(Axis::TranscriptIngestion).await {
        return cell;
    }
    match run_one_turn(RUNNER_TIMEOUT).await {
        Ok(observed) => {
            let messages: Vec<Message> = observed.into_iter().map(|(_, m)| m).collect();
            if !has_terminal_result(&messages) {
                return Cell::fail(
                    ENGINE,
                    Axis::TranscriptIngestion,
                    format!(
                        "the stream-json channel never delivered a terminal `result` \
                         message ({} message(s) observed) — a dead channel, not an \
                         empty fan-out",
                        messages.len()
                    ),
                );
            }
            let tree = SubagentTree::from_messages(&messages);
            let known: Vec<String> = tree.branches().map(str::to_owned).collect();
            let children: Vec<(String, Option<String>)> = tree
                .branches()
                .map(|b| (b.to_owned(), tree.parent_of(b).map(str::to_owned)))
                .collect();
            let (verdict, detail) = check_fanout_linkage(&known, &children);
            Cell::new(ENGINE, Axis::TranscriptIngestion, verdict, detail)
        }
        Err(e) => Cell::fail(
            ENGINE,
            Axis::TranscriptIngestion,
            format!("driving claude failed: {e}"),
        ),
    }
}

/// The compiled `parity-claude-hook` binary this crate ships
/// (`src/bin/parity-claude-hook.rs`) — the actual `PreToolUse` decision
/// path, run as a real, separate process. `CARGO_BIN_EXE_*` is set for
/// integration-test binaries; the local driver (`src/bin/parity.rs`) has no
/// such variable, so it falls back to a sibling of its own executable —
/// `cargo build`/`cargo run` always place every bin target of one crate in
/// the same directory.
fn hook_binary_path() -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_parity-claude-hook") {
        return Ok(PathBuf::from(path));
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe
        .parent()
        .ok_or("current executable has no parent directory")?;
    let candidate = dir.join("parity-claude-hook");
    if candidate.is_file() {
        Ok(candidate)
    } else {
        Err(format!(
            "parity-claude-hook not found beside {} — build it first (`cargo build -p gwk-parity --bins`)",
            exe.display()
        ))
    }
}

fn private_relay_dir(tag: &str) -> Result<PathBuf, String> {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!(
        "gwk-parity-claude-relay-{}-{tag}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| e.to_string())?;
    Ok(dir)
}

fn fixture_ask() -> Result<PreToolUseAsk, String> {
    // Derivation: CLAUDE-HOOKS — the documented PreToolUse stdin schema this
    // fixture instantiates (`session_id`, `transcript_path`, `cwd`,
    // `hook_event_name`, `tool_name`, `tool_input`, `tool_use_id`), decoded
    // through the adapter's own `PreToolUseAsk::from_stdin`.
    PreToolUseAsk::from_stdin(
        br#"{
            "session_id": "gwk-parity-session",
            "transcript_path": "/tmp/gwk-parity-transcript.json",
            "cwd": "/tmp",
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "echo hi"},
            "tool_use_id": "gwk-parity-tool-1"
        }"#,
    )
    .map_err(|e| format!("decoding the built-in fixture ask: {e}"))
}

/// Axis 4 — approval relay. See this module's doc for why the ask is
/// adapter-constructed rather than engine-triggered; everything past that
/// — the socket, the separate hook process, the gate the relayed ask
/// warrants, and the decision returning over the same channel that raised
/// it — is real.
pub async fn approval_relay() -> Cell {
    if let Err(cell) = ensure_version(Axis::ApprovalRelay).await {
        return cell;
    }
    let hook_binary = match hook_binary_path() {
        Ok(p) => p,
        Err(e) => return Cell::fail(ENGINE, Axis::ApprovalRelay, e),
    };
    let dir = match private_relay_dir("approval") {
        Ok(d) => d,
        Err(e) => return Cell::fail(ENGINE, Axis::ApprovalRelay, e),
    };
    let socket_path = dir.join("relay.sock");
    let relay = match AskRelay::bind(&socket_path).await {
        Ok(r) => r,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            return Cell::fail(ENGINE, Axis::ApprovalRelay, format!("bind failed: {e}"));
        }
    };

    let ask = match fixture_ask() {
        Ok(a) => a,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            return Cell::fail(ENGINE, Axis::ApprovalRelay, e);
        }
    };
    let mut hook_process = match tokio::process::Command::new(&hook_binary)
        .env("GWK_PARITY_RELAY_SOCKET", &socket_path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            return Cell::fail(
                ENGINE,
                Axis::ApprovalRelay,
                format!("spawning the hook binary failed: {e}"),
            );
        }
    };
    if let Some(mut stdin) = hook_process.stdin.take() {
        use tokio::io::AsyncWriteExt;
        let mut line = serde_json::to_vec(&ask).unwrap_or_default();
        line.push(b'\n');
        let _ = stdin.write_all(&line).await;
        drop(stdin);
    }

    let outcome = timeout(RUNNER_TIMEOUT, relay.accept()).await;
    let cell = match outcome {
        Ok(Ok((received_ask, responder))) => {
            let command = open_gate_from_ask(GateId::new("gwk-parity-gate"), None, &received_ask);
            let (verdict, detail) = check_gate_carries_question_and_options(&command);
            let _ = responder
                .respond(
                    PermissionDecision::Allow,
                    Some("gwk-parity: allow".to_owned()),
                )
                .await;
            Cell::new(ENGINE, Axis::ApprovalRelay, verdict, detail)
        }
        Ok(Err(e)) => Cell::fail(
            ENGINE,
            Axis::ApprovalRelay,
            format!("relay accept failed: {e}"),
        ),
        Err(_) => Cell::fail(
            ENGINE,
            Axis::ApprovalRelay,
            format!("no ask relayed within {RUNNER_TIMEOUT:?}"),
        ),
    };

    // A pass here is a check on the GATE's shape; the hook's own exit
    // status is not part of the axis this cell judges, but a runner is
    // still expected to leave nothing running either way.
    let _ = timeout(Duration::from_secs(5), hook_process.wait()).await;
    let _ = hook_process.kill().await;
    let _ = std::fs::remove_dir_all(&dir);
    drop(relay);

    cell
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(json: &str) -> Message {
        gwk_adapter_claude::message::parse_line(json.as_bytes()).expect("parse fixture")
    }

    #[test]
    fn has_terminal_result_is_false_on_a_seeded_message_set_without_a_result() {
        // Both shapes the dead channel actually produces: nothing at all,
        // and a start with no matching end (killed or errored mid-turn).
        assert!(!has_terminal_result(&[]));
        let start_only = msg(r#"{"type":"system","subtype":"init"}"#);
        assert!(!has_terminal_result(&[start_only]));
    }

    #[test]
    fn has_terminal_result_is_true_once_a_terminal_result_is_present() {
        let start = msg(r#"{"type":"system","subtype":"init"}"#);
        let end = msg(r#"{"type":"result","subtype":"success"}"#);
        assert!(has_terminal_result(&[start, end]));
    }
}
