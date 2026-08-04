//! opencode runners — driven over `opencode serve`'s HTTP surface via a
//! `curl` subprocess. This workspace pins no HTTP client crate, and a
//! bounded harness dispatch does not get to add one; `curl` is exactly how
//! `gwk-adapter-opencode`'s own module docs say ITS OWN citations were
//! captured ("fetched from a local `opencode 1.18.3` instance (`opencode
//! serve` on a loopback port, `curl .../doc`, then killed...)") — the same
//! tool, the same pattern, one layer further than what got vendored.
//!
//! # The two request shapes the adapter does not (yet) type
//!
//! `gwk-adapter-opencode` normalizes the server's OWN event-bus frames
//! (`event::parse_frame`) and per-child cost rows (`cost::parse_children`)
//! — it has no function for the requests a caller sends to CREATE a session
//! or start a turn (`event.rs`'s own module doc: "whatever host component
//! drives the HTTP client hands this frames"). [`create_session`] and
//! [`prompt_async`] cite the exact source the adapter's own citations
//! already point at — `GET /doc`'s OpenAPI 3.1 schema, fetched the same way
//! from the same pinned instance — for `POST /session` (every field
//! optional, `{}` is a legal body; the response is a `Session` carrying
//! `.id`) and `POST /session/{id}/prompt_async` (`{"parts":
//! [{"type":"text","text":...}]}`, `204` on acceptance, the actual work
//! streaming back over the event bus this crate already knows how to
//! parse). Not an invented shape — the same schema endpoint, read one layer
//! further than what got vendored.
//!
//! # Where this module stops rather than guess
//!
//! [`approval_relay`] does not drive a live-triggered `permission.asked`:
//! the request body that would make a fresh session's own tool calls need
//! approval (`POST /session`'s `permission: PermissionRuleset` field) is
//! not settled by the citation above, and getting it wrong risks a session
//! that runs to completion without ever asking — a silent false pass, not a
//! bounded failure. It instead proves the same shape the OTHER partial
//! runners lean on when a live trigger is out of reach: a real
//! [`PermissionAsk`], built through the adapter's own public struct fields
//! (never hand-encoded wire), producing a real `OpenGate` this axis's check
//! judges for real.

use std::process::Stdio;
use std::time::{Duration, Instant};

use gwk_adapter_opencode::cost::parse_children;
use gwk_adapter_opencode::event::{Event, PermissionAsk, ToolRef, open_gate_command, parse_frame};
use gwk_domain::GateId;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::timeout;

use super::{check_version, version_skip};
use crate::checks::{
    check_fanout_linkage, check_gate_carries_question_and_options, check_lifecycle_observed,
    check_status_skew,
};
use crate::matrix::{Axis, Cell, Engine};
use crate::pins::{OPENCODE_CLI_VERSION, d_bound};

const ENGINE: Engine = Engine::Opencode;

async fn ensure_version(axis: Axis) -> Result<(), Cell> {
    match check_version(gwk_adapter_opencode::ENGINE, OPENCODE_CLI_VERSION).await {
        Ok(()) => Ok(()),
        Err(check) => Err(version_skip(
            ENGINE,
            axis,
            gwk_adapter_opencode::ENGINE,
            check,
        )),
    }
}

/// A port in the private/dynamic range, derived from this process's own id
/// so concurrent runs collide less than a fixed constant would.
// ponytail: no retry-on-bind-conflict loop — a bounded local harness run is
// not a multi-tenant service; a collision is rare enough to re-run past.
fn ephemeral_port() -> u16 {
    40000 + (std::process::id() as u16 % 20000)
}

struct Server {
    child: Child,
    base_url: String,
}

/// How long `spawn_server` polls before giving up on the server ever
/// accepting a connection — discovered live: a flat pre-sleep here (first
/// 400ms, still not enough) raced the real startup time often enough that
/// every event-bus read started against a port nothing was listening on
/// yet, observing nothing.
const READY_POLL_BUDGET: Duration = Duration::from_secs(6);

async fn spawn_server() -> Result<Server, String> {
    let port = ephemeral_port();
    let base_url = format!("http://127.0.0.1:{port}");
    let child = Command::new(gwk_adapter_opencode::ENGINE)
        .args([
            "serve",
            "--port",
            &port.to_string(),
            "--hostname",
            "127.0.0.1",
        ])
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    if !wait_until_ready(&base_url, READY_POLL_BUDGET).await {
        return Err(format!(
            "opencode serve never answered on {base_url} within {READY_POLL_BUDGET:?}"
        ));
    }
    Ok(Server { child, base_url })
}

/// Poll `GET /doc` (a cheap, side-effect-free endpoint every opencode server
/// answers) until it responds or `budget` runs out.
async fn wait_until_ready(base_url: &str, budget: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + budget;
    while tokio::time::Instant::now() < deadline {
        // Derivation: OPENCODE-SERVER — the endpoint table's own `GET /doc`,
        // the schema endpoint every opencode server publishes.
        let probed = Command::new("curl")
            .args([
                "-sS",
                "-o",
                "/dev/null",
                "--max-time",
                "1",
                &format!("{base_url}/doc"),
            ])
            .output()
            .await;
        if matches!(probed, Ok(output) if output.status.success()) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

/// One buffered `curl` call — spawn, wait, capture. For request/response
/// endpoints (`POST /session`, `GET /session/{id}/children`), never for the
/// event bus, which [`stream_events`] reads incrementally instead so a
/// frame's arrival time is real.
async fn curl_once(args: &[&str], budget: Duration) -> Result<Vec<u8>, String> {
    let secs = budget.as_secs().max(1).to_string();
    let mut full_args = vec!["-sS", "--max-time", &secs];
    full_args.extend_from_slice(args);
    let output = timeout(
        budget + Duration::from_secs(2),
        Command::new("curl").args(&full_args).output(),
    )
    .await
    .map_err(|_| "curl timed out".to_owned())?
    .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(format!(
            "curl exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ))
    }
}

async fn create_session(base_url: &str, budget: Duration) -> Result<String, String> {
    // Derivation: OPENCODE-SERVER — the `GET /doc` OpenAPI schema: `POST
    // /session` takes an all-optional body (`{}` is legal) and returns a
    // Session carrying `.id`.
    let body = curl_once(
        &[
            "-X",
            "POST",
            "-H",
            "content-type: application/json",
            "-d",
            "{}",
            &format!("{base_url}/session"),
        ],
        budget,
    )
    .await?;
    let value: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| format!("POST /session did not return JSON: {e}"))?;
    value
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .ok_or_else(|| format!("POST /session response carried no id: {value}"))
}

async fn prompt_async(
    base_url: &str,
    session_id: &str,
    prompt: &str,
    budget: Duration,
) -> Result<(), String> {
    // Derivation: OPENCODE-SERVER — `GET /doc`: `POST
    // /session/{id}/prompt_async` takes `{"parts": [{"type":"text","text":
    // ...}]}` and answers 204 on acceptance, the work streaming back over the
    // event bus.
    let body = serde_json::json!({ "parts": [{ "type": "text", "text": prompt }] }).to_string();
    curl_once(
        &[
            "-X",
            "POST",
            "-H",
            "content-type: application/json",
            "-d",
            &body,
            &format!("{base_url}/session/{session_id}/prompt_async"),
        ],
        budget,
    )
    .await
    .map(|_| ())
}

/// Read `GET /event` incrementally (`curl -N` disables curl's own output
/// buffering) so each frame's arrival gets a real elapsed-since-`started`
/// timestamp — a buffered, wait-for-exit read would collapse every frame to
/// one arrival time, which is not a status-truth measurement, just a
/// deadline. Frame boundaries are the blank line
/// [`gwk_adapter_opencode::event::parse_frame`]'s own doc names; splitting
/// on it is transport framing (plain SSE), not opencode's own protocol —
/// the bytes between two blank lines still go through `parse_frame`
/// unparsed by anything else here.
// Derivation: OPENCODE-SERVER — `GET /event` is the endpoint table's event
// bus; its frames arrive as plain SSE, blank-line separated — the transport
// split done here, with every frame's bytes still parsed only by the adapter.
async fn stream_events(url: &str, started: Instant, budget: Duration) -> Vec<(Duration, Event)> {
    let max_time = (budget.as_secs() + 1).to_string();
    let mut child = match Command::new("curl")
        .args(["-sS", "-N", "--max-time", &max_time, url])
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill().await;
        return Vec::new();
    };
    let mut reader = BufReader::new(stdout);
    let mut collected = Vec::new();
    let mut frame_buf = String::new();
    let read_loop = async {
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => {
                    if line.trim().is_empty() {
                        if !frame_buf.is_empty() {
                            if let Ok(event) = parse_frame(frame_buf.as_bytes()) {
                                collected.push((started.elapsed(), event));
                            }
                            frame_buf.clear();
                        }
                    } else {
                        frame_buf.push_str(&line);
                    }
                }
                Err(_) => break,
            }
        }
    };
    let _ = timeout(budget, read_loop).await;
    let _ = child.kill().await;
    collected
}

/// Axis 1 — lifecycle: the bus's own liveness frame
/// (`docs/PARITY.md`'s inventory notes: "the harness treats that frame as
/// stream liveness, not session state") plus a real `session.created` from
/// a real `POST /session`.
pub async fn lifecycle() -> Cell {
    if let Err(cell) = ensure_version(Axis::Lifecycle).await {
        return cell;
    }
    let mut server = match spawn_server().await {
        Ok(s) => s,
        Err(e) => {
            return Cell::fail(
                ENGINE,
                Axis::Lifecycle,
                format!("spawning `opencode serve` failed: {e}"),
            );
        }
    };
    let started = Instant::now();
    let event_url = format!("{}/event", server.base_url);
    let bus = stream_events(&event_url, started, Duration::from_secs(6));
    let work = async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        create_session(&server.base_url, Duration::from_secs(5)).await
    };
    let (events, session_result) = tokio::join!(bus, work);
    let _ = server.child.kill().await;

    let mut tags = Vec::new();
    if events
        .iter()
        .any(|(_, e)| matches!(e, Event::StreamConnected))
    {
        tags.push("stream_connected".to_owned());
    }
    if events
        .iter()
        .any(|(_, e)| matches!(e, Event::SessionCreated { .. }))
    {
        tags.push("start".to_owned());
    }
    if let Err(e) = session_result {
        return Cell::fail(
            ENGINE,
            Axis::Lifecycle,
            format!("POST /session failed: {e}; bus facts observed before that: {tags:?}"),
        );
    }
    let (verdict, detail) = check_lifecycle_observed(&["stream_connected", "start"], &tags);
    Cell::new(ENGINE, Axis::Lifecycle, verdict, detail)
}

/// Axis 2 — status truth: elapsed time from the bus connecting to the
/// first `session.status`/`session.idle` push, against [`d_bound`].
pub async fn status_truth() -> Cell {
    if let Err(cell) = ensure_version(Axis::StatusTruth).await {
        return cell;
    }
    let mut server = match spawn_server().await {
        Ok(s) => s,
        Err(e) => {
            return Cell::fail(
                ENGINE,
                Axis::StatusTruth,
                format!("spawning `opencode serve` failed: {e}"),
            );
        }
    };
    let started = Instant::now();
    let event_url = format!("{}/event", server.base_url);
    let bus = stream_events(&event_url, started, Duration::from_secs(10));
    let work = async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let session_id = create_session(&server.base_url, Duration::from_secs(5)).await?;
        prompt_async(
            &server.base_url,
            &session_id,
            "Reply with exactly the word: ok.",
            Duration::from_secs(5),
        )
        .await
    };
    let (events, work_result) = tokio::join!(bus, work);
    let _ = server.child.kill().await;
    if let Err(e) = work_result {
        return Cell::fail(ENGINE, Axis::StatusTruth, e);
    }
    let status_event = events.iter().find(|(_, e)| {
        matches!(
            e,
            Event::SessionStatusChanged { .. } | Event::SessionIdle { .. }
        )
    });
    match status_event {
        Some((elapsed, _)) => {
            let observed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
            let (verdict, detail) = check_status_skew(0, observed_ms, d_bound());
            Cell::new(ENGINE, Axis::StatusTruth, verdict, detail)
        }
        None => Cell::fail(
            ENGINE,
            Axis::StatusTruth,
            "no session.status/session.idle event observed",
        ),
    }
}

/// Axis 3 — transcript ingestion (fan-out linkage), read off `GET
/// /session/{id}/children`. This scripted prompt triggers no child session,
/// so the check passes vacuously on real, live-parsed output — proving the
/// create → prompt → fetch → parse → check path, not a fan-out this
/// bounded pass never asks opencode to create.
pub async fn transcript_ingestion() -> Cell {
    if let Err(cell) = ensure_version(Axis::TranscriptIngestion).await {
        return cell;
    }
    let mut server = match spawn_server().await {
        Ok(s) => s,
        Err(e) => {
            return Cell::fail(
                ENGINE,
                Axis::TranscriptIngestion,
                format!("spawning `opencode serve` failed: {e}"),
            );
        }
    };
    let outcome: Result<Vec<u8>, String> = async {
        let session_id = create_session(&server.base_url, Duration::from_secs(5)).await?;
        prompt_async(
            &server.base_url,
            &session_id,
            "Reply with exactly the word: ok.",
            Duration::from_secs(5),
        )
        .await?;
        tokio::time::sleep(Duration::from_secs(2)).await;
        // Derivation: OPENCODE-SERVER — `GET /doc`: `GET
        // /session/{id}/children` lists a session's child sessions; the body
        // is parsed only by the adapter's `parse_children`.
        curl_once(
            &[
                "-X",
                "GET",
                &format!("{}/session/{session_id}/children", server.base_url),
            ],
            Duration::from_secs(5),
        )
        .await
    }
    .await;
    let _ = server.child.kill().await;

    let body = match outcome {
        Ok(b) => b,
        Err(e) => return Cell::fail(ENGINE, Axis::TranscriptIngestion, e),
    };
    let text = String::from_utf8_lossy(&body);
    let children_list = match parse_children(&text) {
        Ok(c) => c,
        Err(e) => {
            return Cell::fail(
                ENGINE,
                Axis::TranscriptIngestion,
                format!("parsing children failed: {e}"),
            );
        }
    };
    let mut known: Vec<String> = children_list.iter().map(|c| c.id.clone()).collect();
    known.extend(children_list.iter().filter_map(|c| c.parent_id.clone()));
    let children: Vec<(String, Option<String>)> = children_list
        .iter()
        .map(|c| (c.id.clone(), c.parent_id.clone()))
        .collect();
    let (verdict, detail) = check_fanout_linkage(&known, &children);
    Cell::new(ENGINE, Axis::TranscriptIngestion, verdict, detail)
}

/// Axis 4 — approval relay. See this module's doc for why the ask is
/// adapter-constructed rather than engine-triggered.
pub async fn approval_relay() -> Cell {
    if let Err(cell) = ensure_version(Axis::ApprovalRelay).await {
        return cell;
    }
    let ask = PermissionAsk {
        request_id: "gwk-parity-perm".to_owned(),
        session_id: "gwk-parity-session".to_owned(),
        permission: "bash".to_owned(),
        patterns: vec!["echo hi".to_owned()],
        tool: Some(ToolRef {
            message_id: "gwk-parity-msg".to_owned(),
            call_id: "gwk-parity-call".to_owned(),
        }),
    };
    let command = open_gate_command(&ask, GateId::new("gwk-parity-gate"), None);
    let (verdict, detail) = check_gate_carries_question_and_options(&command);
    Cell::new(
        ENGINE,
        Axis::ApprovalRelay,
        verdict,
        format!(
            "{detail} (adapter-constructed ask — see this module's doc for why no live session drove it)"
        ),
    )
}
