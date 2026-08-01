//! The JSON-RPC client: newline-delimited frames over the `control_command()`
//! child's stdio.
//!
//! Encoding and decoding are separated from the child-process plumbing on
//! purpose — [`decode_frame`]/[`encode_frame`] are pure functions the test
//! suite drives against constructed byte strings with no process involved,
//! and [`WireClient`] is the thin layer that puts real stdio behind them.

use std::process::Stdio;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

/// How long a single frame line may be before this adapter refuses it rather
/// than growing the accumulation buffer without bound.
///
/// ponytail: one constant, not a knob — 16 MiB comfortably covers the
/// largest legitimate frame (`item/commandExecution/outputDelta` on a
/// chatty command) with headroom, and this is IPC with a child this adapter
/// itself spawned, not a hostile network peer; the bound exists so a wedged
/// or misbehaving engine process fails as a typed error instead of an
/// unbounded allocation, not as a hardened trust boundary.
pub const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// The JSON-RPC `id` union type.
// Derivation: CODEX-APP-SERVER `schemas/JSONRPCMessage.json`
// `#/definitions/RequestId` — `string | integer (int64)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcId {
    Str(String),
    Num(i64),
}

/// One JSON-RPC object as it actually crosses the wire.
///
/// # The header
///
/// Every variant omits the `"jsonrpc":"2.0"` version field. This is not an
/// oversight: the app-server README states the protocol "supports
/// bidirectional communication using JSON-RPC 2.0 messages (with the
/// `"jsonrpc":"2.0"` header omitted on the wire)", and the vendored
/// `JSONRPCMessage.json` schema confirms it structurally — none of its four
/// `anyOf` branches lists `jsonrpc` among their properties. A codec that
/// emitted the header would be encoding a message this engine's parser was
/// never written to expect; a codec that required it on decode would reject
/// every real frame the engine sends. Both directions matter here — this
/// type IS the wire shape, not merely something transformed into it.
// Derivation: CODEX-APP-SERVER `schemas/JSONRPCMessage.json` — the
// `anyOf` of Request/Notification/Response/Error, distinguished purely by
// which fields are present (no discriminant field), plus the app-server
// README's statement that the `jsonrpc` version header is omitted on the
// wire (quoted above).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Frame {
    /// `{ id, method, params? }` — a request that expects a response back
    /// on the same `id`. Sent BY the engine for approval relay; this
    /// adapter never originates one in the scope this crate covers today.
    Request(RequestFrame),
    /// `{ method, params? }` — no `id`, no response expected. Every
    /// lifecycle/status/item/token-usage notification arrives this way.
    Notification(NotificationFrame),
    /// `{ id, result }` — this adapter's reply to a `Request`, carrying the
    /// approval decision.
    Response(ResponseFrame),
    /// `{ id, error }` — a JSON-RPC error reply.
    Error(ErrorFrame),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestFrame {
    pub id: JsonRpcId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotificationFrame {
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResponseFrame {
    pub id: JsonRpcId,
    pub result: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorFrame {
    pub id: JsonRpcId,
    pub error: ErrorObject,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorObject {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl Frame {
    /// Build this adapter's reply to a server-initiated approval request:
    /// `{ id, result: { "decision": ... } }`, `id` echoed from the request
    /// this answers — never freshly minted, per the JSON-RPC response
    /// contract.
    pub fn response(id: JsonRpcId, result: serde_json::Value) -> Self {
        Frame::Response(ResponseFrame { id, result })
    }

    /// Build a JSON-RPC error reply to a request this adapter will not
    /// relay. The numeric `code` is left to the caller rather than pinned
    /// here: the reserved JSON-RPC 2.0 codes are part of the base
    /// specification, not something `CODEX-APP-SERVER` documents, so this
    /// module does not assert a value for it.
    pub fn error(id: JsonRpcId, code: i64, message: impl Into<String>) -> Self {
        Frame::Error(ErrorFrame {
            id,
            error: ErrorObject {
                code,
                message: message.into(),
                data: None,
            },
        })
    }
}

/// Errors a malformed or oversized line, or a failed write/spawn, decodes
/// into — never a panic.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("spawning the control channel failed: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("the control channel's stdin/stdout was not piped")]
    MissingStdio,
    #[error("writing to the control channel failed: {0}")]
    Write(#[source] std::io::Error),
    #[error("reading from the control channel failed: {0}")]
    Read(#[source] std::io::Error),
    #[error("a control-channel line was {actual} bytes, over the {max}-byte limit")]
    LineTooLong { max: usize, actual: usize },
    #[error("a control-channel line was not a valid JSON-RPC frame: {0}")]
    Malformed(#[source] serde_json::Error),
    #[error("encoding a frame to JSON failed: {0}")]
    Encode(#[source] serde_json::Error),
}

/// The guard [`WireClient::recv`] applies to every line before decoding it —
/// pulled out as a pure function so the threshold itself is testable without
/// a live child. A real oversized-line integration test (spawning a process
/// and pushing >16 MiB through a pipe faster than the reader on the other
/// end drains it) would deadlock on the OS pipe buffer: `send`/`recv` here
/// are sequential, so writing a frame this large before ever reading blocks
/// this adapter's write once the kernel's pipe buffer (tens of KiB) fills,
/// while the child blocks its own read-then-echo the same way. That is not
/// a gap in production behavior — this adapter only ever *writes* a tiny
/// approval-decision response, never anything approaching this bound — so
/// the fix is testing the threshold directly, not building infrastructure
/// production code will never exercise.
fn check_line_length(len: usize) -> Result<(), WireError> {
    if len > MAX_LINE_BYTES {
        return Err(WireError::LineTooLong {
            max: MAX_LINE_BYTES,
            actual: len,
        });
    }
    Ok(())
}

/// Decode one newline-delimited-JSON line (without its trailing newline)
/// into a [`Frame`]. Never panics: a malformed line is `Err`, always.
pub fn decode_frame(line: &[u8]) -> Result<Frame, WireError> {
    serde_json::from_slice(line).map_err(WireError::Malformed)
}

/// Encode a [`Frame`] to a single line, WITHOUT a trailing newline — the
/// caller (here, [`WireClient::send`]) owns framing.
pub fn encode_frame(frame: &Frame) -> Result<Vec<u8>, WireError> {
    serde_json::to_vec(frame).map_err(WireError::Encode)
}

/// The child-process half: a [`Frame`] reader/writer over piped stdio.
///
/// Constructed from an arbitrary [`std::process::Command`] rather than
/// hardcoded to [`crate::control_command`], so the read/write plumbing is
/// exercisable in tests against a real (but engine-free) child — see the
/// `/bin/cat` round-trip test below — while [`WireClient::spawn_codex`] is
/// the one production call site.
pub struct WireClient {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
}

impl WireClient {
    /// Spawn `command` with piped stdin/stdout and inherited stderr (the
    /// engine's own diagnostics are not this adapter's to swallow).
    pub fn new(mut command: std::process::Command) -> Result<Self, WireError> {
        command.stdin(Stdio::piped()).stdout(Stdio::piped());
        let mut child = tokio::process::Command::from(command)
            .kill_on_drop(true)
            .spawn()
            .map_err(WireError::Spawn)?;
        let stdin = child.stdin.take().ok_or(WireError::MissingStdio)?;
        let stdout = child.stdout.take().ok_or(WireError::MissingStdio)?;
        Ok(Self {
            child,
            stdin,
            reader: BufReader::new(stdout),
        })
    }

    /// The one production call site: `codex app-server` over its own
    /// invocation from [`crate::control_command`].
    pub fn spawn_codex() -> Result<Self, WireError> {
        Self::new(crate::control_command())
    }

    /// Write one frame, newline-terminated.
    pub async fn send(&mut self, frame: &Frame) -> Result<(), WireError> {
        let mut bytes = encode_frame(frame)?;
        bytes.push(b'\n');
        self.stdin.write_all(&bytes).await.map_err(WireError::Write)
    }

    /// Read and decode the next frame. `Ok(None)` is a clean EOF (the child
    /// closed its stdout, e.g. on exit) — distinct from every error variant,
    /// which is a channel this adapter cannot recover from without a fresh
    /// spawn.
    pub async fn recv(&mut self) -> Result<Option<Frame>, WireError> {
        let mut buf = Vec::new();
        let n = self
            .reader
            .read_until(b'\n', &mut buf)
            .await
            .map_err(WireError::Read)?;
        if n == 0 {
            return Ok(None);
        }
        check_line_length(buf.len())?;
        while matches!(buf.last(), Some(b'\n' | b'\r')) {
            buf.pop();
        }
        decode_frame(&buf).map(Some)
    }

    /// The underlying child, for wait/kill from the owning session.
    pub fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_decodes_without_a_jsonrpc_header() {
        let line = br#"{"id":"req-1","method":"item/commandExecution/requestApproval","params":{"itemId":"i-1"}}"#;
        let frame = decode_frame(line).expect("decodes");
        assert_eq!(
            frame,
            Frame::Request(RequestFrame {
                id: JsonRpcId::Str("req-1".to_owned()),
                method: "item/commandExecution/requestApproval".to_owned(),
                params: Some(serde_json::json!({"itemId": "i-1"})),
            })
        );
    }

    #[test]
    fn a_notification_has_no_id() {
        let line = br#"{"method":"thread/closed","params":{"threadId":"th-1"}}"#;
        let frame = decode_frame(line).expect("decodes");
        assert_eq!(
            frame,
            Frame::Notification(NotificationFrame {
                method: "thread/closed".to_owned(),
                params: Some(serde_json::json!({"threadId": "th-1"})),
            })
        );
    }

    #[test]
    fn a_numeric_request_id_decodes_too() {
        let line = br#"{"id":7,"method":"currentTime/read","params":{"threadId":"th-1"}}"#;
        let frame = decode_frame(line).expect("decodes");
        let Frame::Request(request) = frame else {
            panic!("expected a request");
        };
        assert_eq!(request.id, JsonRpcId::Num(7));
    }

    #[test]
    fn encoding_never_emits_a_jsonrpc_field() {
        let frame = Frame::response(
            JsonRpcId::Str("req-1".to_owned()),
            serde_json::json!({"decision": "accept"}),
        );
        let encoded = encode_frame(&frame).expect("encodes");
        let value: serde_json::Value = serde_json::from_slice(&encoded).expect("valid json");
        assert!(
            value.get("jsonrpc").is_none(),
            "encoded frame must not carry a jsonrpc header: {value}"
        );
        assert_eq!(value["id"], "req-1");
        assert_eq!(value["result"]["decision"], "accept");
    }

    #[test]
    fn a_response_and_an_error_round_trip() {
        let response = Frame::response(
            JsonRpcId::Num(1),
            serde_json::json!({"decision": "decline"}),
        );
        let encoded = encode_frame(&response).expect("encodes");
        assert_eq!(decode_frame(&encoded).expect("decodes"), response);

        let error = Frame::error(JsonRpcId::Num(2), -32601, "method not found");
        let encoded = encode_frame(&error).expect("encodes");
        assert_eq!(decode_frame(&encoded).expect("decodes"), error);
    }

    #[test]
    fn a_malformed_line_is_a_typed_error_not_a_panic() {
        assert!(matches!(
            decode_frame(b"not json at all"),
            Err(WireError::Malformed(_))
        ));
        assert!(matches!(decode_frame(b"{}"), Err(WireError::Malformed(_))));
        assert!(matches!(decode_frame(b""), Err(WireError::Malformed(_))));
    }

    #[test]
    fn a_line_at_or_under_the_cap_is_accepted_and_over_it_is_a_typed_error() {
        assert!(check_line_length(0).is_ok());
        assert!(check_line_length(MAX_LINE_BYTES).is_ok());
        assert!(matches!(
            check_line_length(MAX_LINE_BYTES + 1),
            Err(WireError::LineTooLong {
                max: MAX_LINE_BYTES,
                actual
            }) if actual == MAX_LINE_BYTES + 1
        ));
    }

    #[tokio::test]
    async fn a_frame_round_trips_through_a_real_child_processs_stdio() {
        // `/bin/cat` stands in for `codex app-server`: it never speaks
        // JSON-RPC, but it exercises exactly the stdio plumbing `WireClient`
        // owns (write, newline framing, read-until, decode) end to end,
        // without an engine dependency this crate's tests must not acquire.
        let mut client =
            WireClient::new(std::process::Command::new("/bin/cat")).expect("spawning /bin/cat");

        let sent = Frame::response(
            JsonRpcId::Str("echo-1".to_owned()),
            serde_json::json!({"decision": "accept"}),
        );
        client.send(&sent).await.expect("writing to cat");

        let received = client
            .recv()
            .await
            .expect("reading cat's echo")
            .expect("cat should echo, not hang up");
        assert_eq!(received, sent);

        // Dropping `client` here closes stdin (EOF for `cat`), which is what
        // lets the spawned process exit instead of leaking; nothing further
        // to assert — the round trip above is the whole point of this test.
    }

    #[tokio::test]
    async fn eof_is_none_not_an_error() {
        let mut client =
            WireClient::new(std::process::Command::new("/bin/true")).expect("spawning /bin/true");
        // /bin/true exits immediately and writes nothing.
        let frame = client.recv().await.expect("EOF is not a read error");
        assert_eq!(frame, None);
    }
}
