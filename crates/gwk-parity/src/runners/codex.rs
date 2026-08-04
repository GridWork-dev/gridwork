//! Codex runners — driven over the real `codex app-server` JSON-RPC child
//! through [`gwk_adapter_codex::WireClient`], never a synthetic frame.
//!
//! # The two request shapes the adapter does not (yet) type
//!
//! `gwk-adapter-codex` vendors and normalizes the notifications/requests the
//! ENGINE sends (`item/started`, `thread/status/changed`,
//! `item/commandExecution/requestApproval`, …) — see its `schemas/` and
//! `PROVENANCE.md`. It does not (this being a normalization crate, not yet
//! a session-management one) type the two requests a CALLER sends to start
//! a thread and a turn. Rather than guess, [`run_thread`] cites the exact
//! source `PROVENANCE.md` already vendors from, read one layer further:
//! `codex app-server generate-json-schema`, run against the pinned 0.146.0
//! binary, emits `ClientRequest.json` (names `thread/start` and
//! `turn/start`, each `{id, method, params}`), `v2/ThreadStartParams.json`
//! (every field nullable — `{}` is a legal params body),
//! `v2/ThreadStartResponse.json` (`{thread: {id, ...}}`), and
//! `v2/TurnStartParams.json` (`{threadId, input: [UserInput]}`, required).
//! `UserInput`'s `text` variant is `{"type":"text","text":"..."}` per the
//! same generator's `ClientRequest.json` `#/definitions/UserInput`. This is
//! the SAME command the adapter's own vendoring already runs against the
//! SAME pinned binary — not an invented shape.
//!
//! [`approval_relay`] additionally cites `v2/ThreadStartParams.json`
//! `#/definitions/AskForApproval`'s `"untrusted"` variant and
//! `#/definitions/SandboxMode`'s `"read-only"` variant, from the same
//! generated bundle, to make a trivial shell command actually need approval.
//!
//! One more requirement surfaced only by actually running this live against
//! the pinned binary, not by reading any schema: `thread/start` answers
//! `Not initialized` (JSON-RPC code `-32600`) unless an `initialize` request
//! (`ClientRequest.json`'s own `initialize` variant,
//! `v1/InitializeParams.json`'s `{clientInfo: {name, version}}`) opens the
//! session first. [`run_thread_inner`] sends it before `thread/start`.

use std::time::{Duration, Instant};

use gwk_adapter_codex::CodexAdapter;
use gwk_adapter_codex::schema::ThreadItem;
use gwk_adapter_codex::wire::{Frame, RequestFrame};
use gwk_adapter_codex::{
    ApprovalRequest, CodexEvent, JsonRpcId, WireClient, command_execution_response,
    file_change_response, normalize_notification, normalize_request,
    open_gate_for_command_execution, open_gate_for_file_change,
};
use gwk_domain::engine::{EngineAdapter, LifecycleFact};
use gwk_domain::{GateId, KernelCommand};
use tokio::time::timeout;

use super::{RUNNER_TIMEOUT, check_version, version_skip};
use crate::checks::{
    check_fanout_linkage, check_gate_carries_question_and_options, check_lifecycle_observed,
    check_status_skew,
};
use crate::matrix::{Axis, Cell, Engine};
use crate::pins::{CODEX_CLI_VERSION, d_bound};

const ENGINE: Engine = Engine::Codex;
const PLAIN_PROMPT: &str = "Reply with exactly the word: ok. Do not run any commands.";
const APPROVAL_GATE_ID: &str = "gwk-parity-gate";

async fn ensure_version(axis: Axis) -> Result<(), Cell> {
    match check_version(gwk_adapter_codex::ENGINE, CODEX_CLI_VERSION).await {
        Ok(()) => Ok(()),
        Err(check) => Err(version_skip(ENGINE, axis, gwk_adapter_codex::ENGINE, check)),
    }
}

async fn wait_for_response(
    client: &mut WireClient,
    id: &JsonRpcId,
    budget: Duration,
) -> Result<serde_json::Value, String> {
    let wait = async {
        loop {
            match client.recv().await {
                Ok(Some(Frame::Response(response))) if &response.id == id => {
                    return Ok(response.result);
                }
                Ok(Some(Frame::Error(error))) if &error.id == id => {
                    return Err(format!(
                        "{} (code {})",
                        error.error.message, error.error.code
                    ));
                }
                Ok(Some(_)) => continue,
                Ok(None) => return Err("codex closed the connection".to_owned()),
                Err(e) => return Err(e.to_string()),
            }
        }
    };
    match timeout(budget, wait).await {
        Ok(inner) => inner,
        Err(_) => Err(format!("no response to {id:?} within {budget:?}")),
    }
}

/// Start a thread, start one turn, and collect every notification and
/// server-initiated approval request until `turn/completed` or `budget`
/// runs out. Every approval request seen is answered `accept` (so the turn
/// can proceed) and its `OpenGate` command recorded. The child is always
/// killed on the way out.
async fn run_thread(
    thread_start_params: serde_json::Value,
    prompt: &str,
    budget: Duration,
) -> Result<(Vec<(Duration, CodexEvent)>, Vec<KernelCommand>), String> {
    let mut client = WireClient::spawn_codex().map_err(|e| e.to_string())?;
    let result = run_thread_inner(&mut client, thread_start_params, prompt, budget).await;
    let _ = client.child_mut().kill().await;
    result
}

async fn run_thread_inner(
    client: &mut WireClient,
    thread_start_params: serde_json::Value,
    prompt: &str,
    budget: Duration,
) -> Result<(Vec<(Duration, CodexEvent)>, Vec<KernelCommand>), String> {
    let started = Instant::now();
    // `thread/start` answers `Not initialized` (JSON-RPC code -32600) before
    // the handshake below runs — a real, live-discovered requirement
    // `ClientRequest.json`'s `initialize` variant and `v1/InitializeParams.json`
    // (`{clientInfo: {name, version}}`, `clientInfo` required) name, from the
    // same generated bundle `thread/start`/`turn/start` cite in this module's
    // doc.
    // Derivation: CODEX-APP-SERVER — ClientRequest.json's `initialize` variant
    // + v1/InitializeParams.json: `{clientInfo: {name, version}}`, `clientInfo`
    // required.
    client
        .send(&Frame::Request(RequestFrame {
            id: JsonRpcId::Num(0),
            method: "initialize".to_owned(),
            params: Some(serde_json::json!({
                "clientInfo": { "name": "gwk-parity", "version": env!("CARGO_PKG_VERSION") },
            })),
        }))
        .await
        .map_err(|e| e.to_string())?;
    wait_for_response(client, &JsonRpcId::Num(0), budget).await?;

    // Derivation: CODEX-APP-SERVER — ClientRequest.json names `thread/start`
    // (`{id, method, params}`); v2/ThreadStartParams.json: every field
    // nullable, so a caller-supplied `{}` is a legal params body;
    // v2/ThreadStartResponse.json: `{thread: {id, ...}}` — the `.thread.id`
    // read below.
    client
        .send(&Frame::Request(RequestFrame {
            id: JsonRpcId::Num(1),
            method: "thread/start".to_owned(),
            params: Some(thread_start_params),
        }))
        .await
        .map_err(|e| e.to_string())?;
    let thread_response = wait_for_response(client, &JsonRpcId::Num(1), budget).await?;
    let thread_id = thread_response
        .get("thread")
        .and_then(|t| t.get("id"))
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .ok_or_else(|| "thread/start response carried no thread.id".to_owned())?;

    // Derivation: CODEX-APP-SERVER — v2/TurnStartParams.json: `{threadId,
    // input: [UserInput]}`, both required; UserInput's text variant is
    // `{"type":"text","text":...}` per ClientRequest.json's
    // `#/definitions/UserInput`.
    client
        .send(&Frame::Request(RequestFrame {
            id: JsonRpcId::Num(2),
            method: "turn/start".to_owned(),
            params: Some(serde_json::json!({
                "threadId": thread_id,
                "input": [{"type": "text", "text": prompt}],
            })),
        }))
        .await
        .map_err(|e| e.to_string())?;

    let mut events = Vec::new();
    let mut gates = Vec::new();
    let remaining = budget.saturating_sub(started.elapsed());
    let drive = async {
        loop {
            match client.recv().await {
                Ok(Some(Frame::Notification(notification))) => {
                    if let Ok(event) =
                        normalize_notification(&notification.method, notification.params)
                    {
                        let is_turn_completed = matches!(event, CodexEvent::TurnCompleted { .. });
                        events.push((started.elapsed(), event));
                        if is_turn_completed {
                            break;
                        }
                    }
                }
                Ok(Some(Frame::Request(request))) => {
                    match normalize_request(
                        request.id.clone(),
                        &request.method,
                        request.params.clone(),
                    ) {
                        Ok(approval) => {
                            let (command, response) = match &approval {
                                ApprovalRequest::CommandExecution { id, params } => (
                                    open_gate_for_command_execution(
                                        GateId::new(APPROVAL_GATE_ID),
                                        None,
                                        params,
                                    ),
                                    command_execution_response(id.clone(), "accept"),
                                ),
                                ApprovalRequest::FileChange { id, params } => (
                                    open_gate_for_file_change(
                                        GateId::new(APPROVAL_GATE_ID),
                                        None,
                                        params,
                                    ),
                                    file_change_response(id.clone(), "accept"),
                                ),
                            };
                            gates.push(command);
                            if let Ok(frame) = response {
                                let _ = client.send(&frame).await;
                            }
                        }
                        Err(_) => {
                            // A server-initiated request this adapter does
                            // not relay: answer with an error so codex's
                            // own caller does not hang on a reply that will
                            // never come (event.rs's own contract for
                            // NormalizeError::UnsupportedRequest).
                            let _ = client
                                .send(&Frame::error(
                                    request.id,
                                    -32601,
                                    "gwk-parity does not relay this method",
                                ))
                                .await;
                        }
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
    };
    let _ = timeout(remaining, drive).await;

    Ok((events, gates))
}

/// Axis 1 — lifecycle. Reaching `run_thread`'s `Ok` at all means the
/// `thread/start` response arrived (the only way `run_thread_inner` learns
/// the thread id it needs for `turn/start`), so "start" is satisfied by the
/// response itself; "end" requires the `turn/completed` notification this
/// crate's own drive loop breaks on.
pub async fn lifecycle() -> Cell {
    if let Err(cell) = ensure_version(Axis::Lifecycle).await {
        return cell;
    }
    match run_thread(serde_json::json!({}), PLAIN_PROMPT, RUNNER_TIMEOUT).await {
        Ok((events, _)) => {
            // Through the adapter, not around it — same reason as the claude
            // runner. Note what the old spelling hid: `start` was pushed
            // unconditionally, so the cell could pass without a `thread/started`
            // ever arriving. It has to be observed now.
            let facts: Vec<LifecycleFact> = events
                .iter()
                .filter_map(|(_, e)| CodexAdapter.normalize(e.clone()).ok())
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
            format!("driving codex failed: {e}"),
        ),
    }
}

/// Axis 2 — status truth: elapsed time from `turn/start` to the first
/// notification the adapter normalized, against [`d_bound`].
pub async fn status_truth() -> Cell {
    if let Err(cell) = ensure_version(Axis::StatusTruth).await {
        return cell;
    }
    match run_thread(serde_json::json!({}), PLAIN_PROMPT, RUNNER_TIMEOUT).await {
        Ok((events, _)) => match events.first() {
            Some((elapsed, _)) => {
                let observed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
                let (verdict, detail) = check_status_skew(0, observed_ms, d_bound());
                Cell::new(ENGINE, Axis::StatusTruth, verdict, detail)
            }
            None => Cell::fail(
                ENGINE,
                Axis::StatusTruth,
                "no notification arrived to measure skew against",
            ),
        },
        Err(e) => Cell::fail(
            ENGINE,
            Axis::StatusTruth,
            format!("driving codex failed: {e}"),
        ),
    }
}

/// Axis 3 — transcript ingestion (fan-out linkage), read off
/// `collabAgentToolCall` items — the item `docs/PARITY.md` axis 3 names
/// explicitly for codex's per-child status. This scripted prompt triggers
/// no fan-out, so the check passes vacuously on real, live-parsed events —
/// proving the collect → normalize → check path, not a fan-out this bounded
/// pass never asks codex to create.
pub async fn transcript_ingestion() -> Cell {
    if let Err(cell) = ensure_version(Axis::TranscriptIngestion).await {
        return cell;
    }
    match run_thread(serde_json::json!({}), PLAIN_PROMPT, RUNNER_TIMEOUT).await {
        Ok((events, _)) => {
            let mut known = Vec::new();
            let mut children = Vec::new();
            for (_, event) in &events {
                let item = match event {
                    CodexEvent::ItemStarted { item, .. }
                    | CodexEvent::ItemCompleted { item, .. } => Some(item),
                    _ => None,
                };
                if let Some(ThreadItem::CollabAgentToolCall {
                    sender_thread_id,
                    receiver_thread_ids,
                    ..
                }) = item
                {
                    known.push(sender_thread_id.clone());
                    for child in receiver_thread_ids {
                        known.push(child.clone());
                        children.push((child.clone(), Some(sender_thread_id.clone())));
                    }
                }
            }
            let (verdict, detail) = check_fanout_linkage(&known, &children);
            Cell::new(ENGINE, Axis::TranscriptIngestion, verdict, detail)
        }
        Err(e) => Cell::fail(
            ENGINE,
            Axis::TranscriptIngestion,
            format!("driving codex failed: {e}"),
        ),
    }
}

/// Axis 4 — approval relay: `sandbox: read-only` + `approvalPolicy:
/// untrusted` makes even a trivial shell command need approval, so this run
/// is genuinely engine-triggered, not adapter-constructed.
pub async fn approval_relay() -> Cell {
    if let Err(cell) = ensure_version(Axis::ApprovalRelay).await {
        return cell;
    }
    // Derivation: CODEX-APP-SERVER — v2/ThreadStartParams.json's
    // `#/definitions/AskForApproval` `"untrusted"` and
    // `#/definitions/SandboxMode` `"read-only"` variants.
    let thread_start_params = serde_json::json!({
        "sandbox": "read-only",
        "approvalPolicy": "untrusted",
    });
    // A plain `echo hi` needs no escalation even under `untrusted` — codex
    // treats a side-effect-free read as already safe. A file WRITE under a
    // read-only sandbox is the reliable trigger; discovered live, not
    // assumed.
    let prompt = "Use your shell/exec tool to run: echo hi > /tmp/gwk-parity-approval-probe.txt";
    match run_thread(thread_start_params, prompt, RUNNER_TIMEOUT).await {
        Ok((events, gates)) => match gates.first() {
            Some(command) => {
                let (verdict, detail) = check_gate_carries_question_and_options(command);
                Cell::new(ENGINE, Axis::ApprovalRelay, verdict, detail)
            }
            None => {
                let methods: Vec<&str> = events
                    .iter()
                    .map(|(_, e)| match e {
                        CodexEvent::Unrecognized { method } => method.as_str(),
                        _ => "(normalized)",
                    })
                    .collect();
                Cell::fail(
                    ENGINE,
                    Axis::ApprovalRelay,
                    format!(
                        "no commandExecution/fileChange approval request arrived within the budget; \
                         {} event(s) observed instead: {methods:?}",
                        events.len()
                    ),
                )
            }
        },
        Err(e) => Cell::fail(
            ENGINE,
            Axis::ApprovalRelay,
            format!("driving codex failed: {e}"),
        ),
    }
}
