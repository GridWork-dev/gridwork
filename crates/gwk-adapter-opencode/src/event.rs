//! Normalized events off the opencode server's event bus (`GET /event`).
//!
//! The server multiplexes lifecycle, status, and permission notifications
//! onto one SSE stream. This module turns one frame's bytes into gwk's own
//! [`Event`] — it never opens the connection itself; whatever host
//! component drives the HTTP client hands this frames, and this is where
//! frames become typed facts the kernel side can act on.
//!
//! Two permitted sources cover this module: `OPENCODE-PLUGINS`
//! (opencode.ai/docs/plugins) for the typed event-bus names, and
//! `OPENCODE-SERVER` (opencode.ai/docs/server) for everything shaped by the
//! OpenAPI 3.1 schema that page's server publishes at `GET /doc`. The
//! field-level shapes below were read directly out of that schema — fetched
//! from a local `opencode 1.18.3` instance (`opencode serve` on a loopback
//! port, `curl .../doc`, then killed; no session was ever created) rather
//! than assumed. Residual honestly kept: a schema describes what the wire
//! *can* carry, not that a real running engine exercises every shape this
//! crate normalizes — that is what the parity harness proves live, per
//! `docs/PARITY.md`.

use serde::Deserialize;
use serde_json::Value;

use gwk_domain::{AttemptId, GateId, KernelCommand};

/// Largest SSE frame this adapter accepts. Its own risk tolerance, not a
/// bound the protocol negotiates.
// ponytail: a flat cap, not tuned against a real payload distribution —
// raise it if a legitimate frame ever needs more room.
pub const MAX_EVENT_FRAME_BYTES: usize = 1024 * 1024;

/// Why a raw SSE frame did not become an [`Event`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EventParseError {
    #[error("event frame exceeds {MAX_EVENT_FRAME_BYTES} bytes")]
    TooLarge,
    #[error("frame carries no `data:` line")]
    NoData,
    #[error("event frame is not valid UTF-8")]
    NotUtf8,
    #[error("malformed event JSON: {0}")]
    Malformed(String),
}

/// One `data:` payload off the bus, before this crate narrows it to an
/// [`Event`] it normalizes.
// Derivation: OPENCODE-SERVER §Events (OpenAPI schema at `GET /doc`) — every
// bus event's schema (`EventSessionCreated`, `EventPermissionAsked`, …) is
// `{ "id": string, "type": string, "properties": object }`; this crate reads
// `type` and `properties`, the two fields it needs.
#[derive(Debug, Clone, Deserialize)]
struct BusEnvelope {
    r#type: String,
    #[serde(default)]
    properties: Value,
}

/// A session's live status: pushed as `session.status` and polled at
/// `GET /session/status`.
// Derivation: OPENCODE-SERVER §Sessions (OpenAPI schema at `GET /doc`) —
// `GET /session/status` returns `{ [sessionID]: SessionStatus }`; the
// `SessionStatus` schema is a `type`-tagged union of three objects: `{type:
// "idle"}`, `{type: "busy"}`, and `{type: "retry", attempt, message, next}`
// (`retry` also carries an optional `action`, not modeled here).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionStatus {
    Idle,
    Busy,
    Retry {
        attempt: u64,
        message: String,
        next: u64,
    },
}

/// The tool call a `permission.asked` event is about — two correlation ids,
/// never a human-readable name.
// Derivation: OPENCODE-SERVER §Permissions (OpenAPI schema at `GET /doc`) —
// `PermissionRequest.tool`: `{ messageID, callID }`, both required when
// `tool` itself is present.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ToolRef {
    #[serde(rename = "messageID")]
    pub message_id: String,
    #[serde(rename = "callID")]
    pub call_id: String,
}

/// What a `permission.asked` event reported. There is no `question` field on
/// the wire — the adapter builds one from `permission` and `patterns`, in
/// [`PermissionAsk::question`].
// Derivation: OPENCODE-SERVER §Permissions (OpenAPI schema at `GET /doc`) —
// `PermissionRequest`'s fields: `id`, `sessionID`, `permission` (what is
// being asked about, e.g. `bash`), `patterns`, and an optional `tool`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PermissionAsk {
    #[serde(rename = "id")]
    pub request_id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    pub permission: String,
    pub patterns: Vec<String>,
    #[serde(default)]
    pub tool: Option<ToolRef>,
}

impl PermissionAsk {
    /// A human-readable prompt built from what the engine reported, to carry
    /// as `OpenGate`'s `question` — the relay transports the engine's own
    /// words nowhere on the wire, so this is where they come from.
    pub fn question(&self) -> String {
        if self.patterns.is_empty() {
            format!("Allow `{}`?", self.permission)
        } else {
            format!(
                "Allow `{}` matching {}?",
                self.permission,
                self.patterns.join(", ")
            )
        }
    }
}

/// One of opencode's eight typed session-error shapes, reduced to what every
/// one of them can carry.
// Derivation: OPENCODE-SERVER §Sessions (OpenAPI schema at `GET /doc`) —
// every `session.error` variant (`ProviderAuthError`, `UnknownError`,
// `MessageOutputLengthError`, `MessageAbortedError`, `StructuredOutputError`,
// `ContextOverflowError`, `ContentFilterError`, `APIError`) is `{ name,
// data }`; `data.message` is present on seven of the eight and absent only
// on `MessageOutputLengthError`, whose `data` has no properties at all.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SessionErrorInfo {
    pub name: String,
    pub data: SessionErrorData,
}

#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
pub struct SessionErrorData {
    #[serde(default)]
    pub message: Option<String>,
}

/// The normalized fact one bus frame carries, attributable to the session it
/// names.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The stream's own liveness, not a fact about any session.
    ///
    /// Anything keying readiness on this event is keying on `GET /event`
    /// specifically — never on `GET /global/event`.
    // Derivation: OPENCODE-SERVER §Events — `GET /event`: "First event is
    // `server.connected`, then bus events." Stated for that stream ONLY:
    // the same endpoint table lists `GET /global/event` with no first-event
    // sentence at all, so nothing here — or downstream of here — may treat
    // the global stream as opening with `server.connected`.
    StreamConnected,
    SessionCreated {
        session_id: String,
        parent_id: Option<String>,
    },
    SessionIdle {
        session_id: String,
    },
    SessionError {
        session_id: String,
        error: Option<SessionErrorInfo>,
    },
    SessionDeleted {
        session_id: String,
    },
    SessionStatusChanged {
        session_id: String,
        status: SessionStatus,
    },
    PermissionAsked(PermissionAsk),
    PermissionReplied {
        session_id: String,
        request_id: String,
        reply: PermissionDecision,
    },
    /// A bus event this adapter does not normalize further (opencode's set
    /// is open and covers far more than lifecycle/status/permission — file,
    /// tool, message-part, TUI events, and more). Kept rather than dropped,
    /// so a caller can see the stream stayed alive.
    Other {
        r#type: String,
    },
}

fn require_str(props: &Value, key: &'static str) -> Result<String, EventParseError> {
    props
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| EventParseError::Malformed(format!("missing `{key}`")))
}

// Derivation: OPENCODE-PLUGINS — the event-bus type names matched below
// (`session.created`, `session.idle`, `session.error`, `session.deleted`,
// `session.status`, `permission.asked`, `permission.replied`) are drawn from
// the Session Events and Permission Events lists.
// Derivation: OPENCODE-SERVER §Sessions/Permissions (OpenAPI schema at
// `GET /doc`) — each matched event's property shape: `session.created` and
// `session.deleted` carry `{ sessionID, info: Session }` (`parent_id` reads
// `info.parentID`, since `Session` has no top-level `parentID` on the event
// itself); `session.error` carries `{ sessionID, error? }`; `session.status`
// carries `{ sessionID, status: SessionStatus }`; `permission.replied`
// carries `{ sessionID, requestID, reply }`.
fn normalize(envelope: BusEnvelope) -> Result<Event, EventParseError> {
    let props = envelope.properties;
    match envelope.r#type.as_str() {
        "server.connected" => Ok(Event::StreamConnected),
        "session.created" => {
            let parent_id = props
                .get("info")
                .and_then(|info| info.get("parentID"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            Ok(Event::SessionCreated {
                session_id: require_str(&props, "sessionID")?,
                parent_id,
            })
        }
        "session.idle" => Ok(Event::SessionIdle {
            session_id: require_str(&props, "sessionID")?,
        }),
        "session.error" => {
            let error = match props.get("error") {
                Some(value) if !value.is_null() => Some(
                    serde_json::from_value(value.clone())
                        .map_err(|e| EventParseError::Malformed(e.to_string()))?,
                ),
                _ => None,
            };
            Ok(Event::SessionError {
                session_id: require_str(&props, "sessionID")?,
                error,
            })
        }
        "session.deleted" => Ok(Event::SessionDeleted {
            session_id: require_str(&props, "sessionID")?,
        }),
        "session.status" => {
            let status_value = props.get("status").cloned().unwrap_or(Value::Null);
            let status: SessionStatus = serde_json::from_value(status_value)
                .map_err(|e| EventParseError::Malformed(e.to_string()))?;
            Ok(Event::SessionStatusChanged {
                session_id: require_str(&props, "sessionID")?,
                status,
            })
        }
        "permission.asked" => {
            let ask: PermissionAsk = serde_json::from_value(props)
                .map_err(|e| EventParseError::Malformed(e.to_string()))?;
            Ok(Event::PermissionAsked(ask))
        }
        "permission.replied" => {
            let reply = require_str(&props, "reply")?
                .parse::<PermissionDecision>()
                .map_err(|e| EventParseError::Malformed(e.to_string()))?;
            Ok(Event::PermissionReplied {
                session_id: require_str(&props, "sessionID")?,
                request_id: require_str(&props, "requestID")?,
                reply,
            })
        }
        other => Ok(Event::Other {
            r#type: other.to_owned(),
        }),
    }
}

/// Parse one SSE frame's bytes — everything between two blank lines — into
/// the fact it carries.
///
/// SSE's `data:` line framing is the public wire format `GET /event` speaks
/// and is not opencode-specific — the JSON envelope each `data:` payload
/// carries, and every event name this recognizes, are [`BusEnvelope`] and
/// [`normalize`]'s citations above.
pub fn parse_frame(frame: &[u8]) -> Result<Event, EventParseError> {
    if frame.len() > MAX_EVENT_FRAME_BYTES {
        return Err(EventParseError::TooLarge);
    }
    let text = std::str::from_utf8(frame).map_err(|_| EventParseError::NotUtf8)?;
    let data = text
        .lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim_start)
        .collect::<Vec<_>>()
        .join("\n");
    if data.is_empty() {
        return Err(EventParseError::NoData);
    }
    let envelope: BusEnvelope =
        serde_json::from_str(&data).map_err(|e| EventParseError::Malformed(e.to_string()))?;
    normalize(envelope)
}

/// The three replies the reply endpoint accepts.
// Derivation: OPENCODE-SERVER §Permissions (OpenAPI schema at `GET /doc`) —
// `POST /permission/{requestID}/reply`'s request body: `reply` is one of
// `once`, `always`, `reject`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    Once,
    Always,
    Reject,
}

/// A `chosen_option` naming none of [`PermissionDecision::ALL`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("unknown permission decision `{0}` — expected once, always, or reject")]
pub struct UnknownDecision(String);

impl PermissionDecision {
    pub const ALL: [Self; 3] = [Self::Once, Self::Always, Self::Reject];

    pub const fn as_wire_str(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Always => "always",
            Self::Reject => "reject",
        }
    }
}

impl std::str::FromStr for PermissionDecision {
    type Err = UnknownDecision;

    /// Parse a `DecideGate` command's `chosen_option` — always one of
    /// [`Self::as_wire_str`]'s three values, since [`open_gate_command`]
    /// never offers any other option.
    fn from_str(chosen_option: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|decision| decision.as_wire_str() == chosen_option)
            .ok_or_else(|| UnknownDecision(chosen_option.to_owned()))
    }
}

/// Build the `OpenGate` command a `permission.asked` event warrants. The
/// caller supplies the kernel-scoped identifiers — `gate_id`, `attempt_id`
/// — since this adapter knows only what the engine said, never how the
/// kernel names its own aggregates.
pub fn open_gate_command(
    ask: &PermissionAsk,
    gate_id: GateId,
    attempt_id: Option<AttemptId>,
) -> KernelCommand {
    KernelCommand::OpenGate {
        gate_id,
        attempt_id,
        phase_ref: None,
        kind: Some("opencode_permission".to_owned()),
        question: Some(ask.question()),
        options: Some(
            PermissionDecision::ALL
                .into_iter()
                .map(|decision| decision.as_wire_str().to_owned())
                .collect(),
        ),
    }
}

/// The HTTP action to perform when a `DecideGate` verdict comes back —
/// constructed, never sent. This crate has no HTTP client and needs none to
/// be correct: whatever host component owns the opencode connection issues
/// the request this describes.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionReply {
    pub method: &'static str,
    pub path: String,
    pub body: Value,
}

/// Build the reply for a decided gate. `request_id` comes off the
/// [`PermissionAsk`] that raised it — the relay answers through the same
/// channel that asked, per this adapter's own contract.
// Derivation: OPENCODE-SERVER §Permissions (OpenAPI schema at `GET /doc`) —
// the current, non-deprecated permission-reply endpoint is `POST
// /permission/{requestID}/reply` (operation `permission.reply`), body `{
// "reply": <decision> }`, no `sessionID` in its path. The schema separately
// documents `POST /session/{sessionID}/permissions/{permissionID}`
// (operation `permission.respond`, body `{ "response": <decision> }`)
// explicitly flagged `deprecated: true`, and a `/api/session/{sessionID}
// /permission/{requestID}/reply` v2 surface (body `{ "reply": <decision> }`)
// this adapter does not use: the events it normalizes above are the v1 pair
// `permission.asked`/`permission.replied` — v2's are separately named
// `permission.v2.asked`/`permission.v2.replied` in the same schema, and
// mixing a v1 ask with a v2 reply route would answer through a channel that
// never raised it.
pub fn reply_action(request_id: &str, decision: PermissionDecision) -> PermissionReply {
    PermissionReply {
        method: "POST",
        path: format!("/permission/{request_id}/reply"),
        body: serde_json::json!({ "reply": decision.as_wire_str() }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(json: &str) -> Vec<u8> {
        format!("event: message\ndata: {json}\n\n").into_bytes()
    }

    #[test]
    fn server_connected_is_stream_liveness_not_session_state() {
        let event = parse_frame(&frame(r#"{"type":"server.connected"}"#)).expect("parses");
        assert_eq!(event, Event::StreamConnected);
    }

    #[test]
    fn every_lifecycle_event_normalizes_and_attributes_its_session() {
        let created = parse_frame(&frame(
            r#"{"type":"session.created","properties":{"sessionID":"ses_1","info":{"id":"ses_1","parentID":"ses_0"}}}"#,
        ))
        .expect("parses");
        assert_eq!(
            created,
            Event::SessionCreated {
                session_id: "ses_1".to_owned(),
                parent_id: Some("ses_0".to_owned()),
            }
        );

        let idle = parse_frame(&frame(
            r#"{"type":"session.idle","properties":{"sessionID":"ses_1"}}"#,
        ))
        .expect("parses");
        assert_eq!(
            idle,
            Event::SessionIdle {
                session_id: "ses_1".to_owned()
            }
        );

        let error = parse_frame(&frame(
            r#"{"type":"session.error","properties":{"sessionID":"ses_1","error":{"name":"UnknownError","data":{"message":"boom"}}}}"#,
        ))
        .expect("parses");
        assert_eq!(
            error,
            Event::SessionError {
                session_id: "ses_1".to_owned(),
                error: Some(SessionErrorInfo {
                    name: "UnknownError".to_owned(),
                    data: SessionErrorData {
                        message: Some("boom".to_owned())
                    },
                }),
            }
        );

        // MessageOutputLengthError's `data` carries no `message` at all —
        // the one variant the schema does not require it on.
        let no_message = parse_frame(&frame(
            r#"{"type":"session.error","properties":{"sessionID":"ses_1","error":{"name":"MessageOutputLengthError","data":{}}}}"#,
        ))
        .expect("parses");
        assert_eq!(
            no_message,
            Event::SessionError {
                session_id: "ses_1".to_owned(),
                error: Some(SessionErrorInfo {
                    name: "MessageOutputLengthError".to_owned(),
                    data: SessionErrorData { message: None },
                }),
            }
        );

        let deleted = parse_frame(&frame(
            r#"{"type":"session.deleted","properties":{"sessionID":"ses_1","info":{"id":"ses_1"}}}"#,
        ))
        .expect("parses");
        assert_eq!(
            deleted,
            Event::SessionDeleted {
                session_id: "ses_1".to_owned()
            }
        );
    }

    #[test]
    fn session_status_push_carries_the_three_states() {
        let idle = parse_frame(&frame(
            r#"{"type":"session.status","properties":{"sessionID":"ses_1","status":{"type":"idle"}}}"#,
        ))
        .expect("parses");
        assert_eq!(
            idle,
            Event::SessionStatusChanged {
                session_id: "ses_1".to_owned(),
                status: SessionStatus::Idle,
            }
        );

        let busy = parse_frame(&frame(
            r#"{"type":"session.status","properties":{"sessionID":"ses_1","status":{"type":"busy"}}}"#,
        ))
        .expect("parses");
        assert_eq!(
            busy,
            Event::SessionStatusChanged {
                session_id: "ses_1".to_owned(),
                status: SessionStatus::Busy,
            }
        );

        let retry = parse_frame(&frame(
            r#"{"type":"session.status","properties":{"sessionID":"ses_1","status":{"type":"retry","attempt":2,"message":"rate limited","next":5000}}}"#,
        ))
        .expect("parses");
        assert_eq!(
            retry,
            Event::SessionStatusChanged {
                session_id: "ses_1".to_owned(),
                status: SessionStatus::Retry {
                    attempt: 2,
                    message: "rate limited".to_owned(),
                    next: 5000,
                },
            }
        );
    }

    #[test]
    fn permission_asked_becomes_open_gate_with_the_engines_question_and_the_reply_options() {
        let event = parse_frame(&frame(
            r#"{"type":"permission.asked","properties":{"id":"perm_1","sessionID":"ses_1","permission":"bash","patterns":["rm -rf *"],"tool":{"messageID":"msg_1","callID":"call_1"}}}"#,
        ))
        .expect("parses");
        let Event::PermissionAsked(ask) = event else {
            panic!("expected PermissionAsked");
        };
        assert_eq!(ask.request_id, "perm_1");
        assert_eq!(ask.session_id, "ses_1");
        assert_eq!(
            ask.tool,
            Some(ToolRef {
                message_id: "msg_1".to_owned(),
                call_id: "call_1".to_owned(),
            })
        );

        let command = open_gate_command(&ask, GateId::new("gate-1"), Some(AttemptId::new("att-1")));
        let KernelCommand::OpenGate {
            question, options, ..
        } = command
        else {
            panic!("expected OpenGate");
        };
        assert_eq!(question, Some("Allow `bash` matching rm -rf *?".to_owned()));
        assert_eq!(
            options,
            Some(vec![
                "once".to_owned(),
                "always".to_owned(),
                "reject".to_owned()
            ])
        );
    }

    #[test]
    fn a_permission_ask_without_a_tool_call_still_parses() {
        let event = parse_frame(&frame(
            r#"{"type":"permission.asked","properties":{"id":"perm_1","sessionID":"ses_1","permission":"webfetch","patterns":[]}}"#,
        ))
        .expect("parses");
        let Event::PermissionAsked(ask) = event else {
            panic!("expected PermissionAsked");
        };
        assert_eq!(ask.tool, None);
        assert_eq!(ask.question(), "Allow `webfetch`?");
    }

    #[test]
    fn permission_replied_carries_the_request_and_the_decision_it_answered() {
        let event = parse_frame(&frame(
            r#"{"type":"permission.replied","properties":{"sessionID":"ses_1","requestID":"perm_1","reply":"once"}}"#,
        ))
        .expect("parses");
        assert_eq!(
            event,
            Event::PermissionReplied {
                session_id: "ses_1".to_owned(),
                request_id: "perm_1".to_owned(),
                reply: PermissionDecision::Once,
            }
        );
    }

    #[test]
    fn an_unrecognized_event_type_is_kept_as_other_not_dropped() {
        let event =
            parse_frame(&frame(r#"{"type":"message.updated","properties":{}}"#)).expect("parses");
        assert_eq!(
            event,
            Event::Other {
                r#type: "message.updated".to_owned()
            }
        );
    }

    #[test]
    fn each_decision_maps_from_its_chosen_option_and_back() {
        for decision in PermissionDecision::ALL {
            let wire = decision.as_wire_str();
            assert_eq!(wire.parse::<PermissionDecision>(), Ok(decision));
        }
        assert_eq!(
            "bogus".parse::<PermissionDecision>(),
            Err(UnknownDecision("bogus".to_owned()))
        );
    }

    #[test]
    fn reply_action_targets_the_v1_reply_endpoint_with_the_reply_body_key() {
        let reply = reply_action("perm_1", PermissionDecision::Once);
        assert_eq!(reply.method, "POST");
        assert_eq!(reply.path, "/permission/perm_1/reply");
        assert_eq!(reply.body, serde_json::json!({ "reply": "once" }));
    }

    #[test]
    fn malformed_event_json_is_a_typed_error_not_a_panic() {
        assert!(matches!(
            parse_frame(b"event: message\ndata: {not json}\n\n"),
            Err(EventParseError::Malformed(_))
        ));
        assert!(matches!(
            parse_frame(&frame(r#"{"type":"session.idle","properties":{}}"#)),
            Err(EventParseError::Malformed(_))
        ));
        assert!(matches!(
            parse_frame(&frame(
                r#"{"type":"permission.asked","properties":{"id":"perm_1","sessionID":"ses_1","tool":"bash"}}"#
            )),
            Err(EventParseError::Malformed(_))
        ));
    }

    #[test]
    fn a_frame_with_no_data_line_is_refused() {
        assert_eq!(
            parse_frame(b"event: message\n\n"),
            Err(EventParseError::NoData)
        );
    }

    #[test]
    fn an_oversized_frame_is_refused_before_it_is_parsed() {
        let huge = vec![b'x'; MAX_EVENT_FRAME_BYTES + 1];
        assert_eq!(parse_frame(&huge), Err(EventParseError::TooLarge));
    }
}
