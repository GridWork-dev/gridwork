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
//! OpenAPI 3.1 schema that page's server publishes at `GET /doc` — the
//! `{ "type", "properties" }` envelope, `permission.asked`'s property
//! fields, `SessionStatus`'s three values, and the current permission-reply
//! endpoint's path and body. Residual honestly kept: this crate never talks
//! to a live engine, so these field-level shapes are this dispatch's best
//! reading of the schema, not an independent re-derivation against a running
//! server — the parity harness settles that live, per `docs/PARITY.md`.

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
// bus event arrives as a `{ "type": string, "properties": object }` envelope.
#[derive(Debug, Clone, Deserialize)]
struct BusEnvelope {
    r#type: String,
    #[serde(default)]
    properties: Value,
}

/// A session's live status: pushed as `session.status` and polled at
/// `GET /session/status`.
// Derivation: OPENCODE-SERVER §Sessions — `GET /session/status` returns
// `{ [sessionID]: SessionStatus }`, confirming the endpoint and its
// per-session map shape; the `SessionStatus` schema's three values
// (`idle`/`busy`/`retry`) are the OpenAPI schema `GET /doc` publishes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Idle,
    Busy,
    Retry,
}

/// What a `permission.asked` event reported. There is no `question` field on
/// the wire — the adapter builds one from the tool call and patterns the
/// engine named, in [`PermissionAsk::question`].
// Derivation: OPENCODE-SERVER §Permissions (OpenAPI schema at `GET /doc`) —
// the `permission.asked` event's property fields: an `id`, a `sessionID`,
// and the tool call being asked about (`tool`, `patterns`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PermissionAsk {
    #[serde(rename = "id")]
    pub request_id: String,
    #[serde(rename = "sessionID")]
    pub session_id: String,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub patterns: Vec<String>,
}

impl PermissionAsk {
    /// A human-readable prompt built from what the engine reported, to carry
    /// as `OpenGate`'s `question` — the relay transports the engine's own
    /// words nowhere on the wire, so this is where they come from.
    pub fn question(&self) -> String {
        match (self.tool.as_deref(), self.patterns.is_empty()) {
            (Some(tool), false) => format!("Allow `{tool}` matching {}?", self.patterns.join(", ")),
            (Some(tool), true) => format!("Allow `{tool}`?"),
            (None, false) => format!("Allow an action matching {}?", self.patterns.join(", ")),
            (None, true) => "Allow this action?".to_owned(),
        }
    }
}

/// The normalized fact one bus frame carries, attributable to the session it
/// names.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The stream's own liveness, not a fact about any session.
    // Derivation: OPENCODE-SERVER §Events — `GET /event`: "First event is
    // `server.connected`, then bus events."
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
        message: Option<String>,
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

fn optional_str(props: &Value, key: &'static str) -> Option<String> {
    props.get(key).and_then(Value::as_str).map(str::to_owned)
}

// Derivation: OPENCODE-PLUGINS — the event-bus type names matched below
// (`session.created`, `session.idle`, `session.error`, `session.deleted`,
// `session.status`, `permission.asked`, `permission.replied`) are drawn from
// the Session Events and Permission Events lists.
// Derivation: OPENCODE-SERVER §Sessions/Permissions (OpenAPI schema at
// `GET /doc`) — each matched event's property field names (`sessionID`,
// `parentID`, `message`, `status`, `id`) come from that schema.
fn normalize(envelope: BusEnvelope) -> Result<Event, EventParseError> {
    let props = envelope.properties;
    match envelope.r#type.as_str() {
        "server.connected" => Ok(Event::StreamConnected),
        "session.created" => Ok(Event::SessionCreated {
            session_id: require_str(&props, "sessionID")?,
            parent_id: optional_str(&props, "parentID"),
        }),
        "session.idle" => Ok(Event::SessionIdle {
            session_id: require_str(&props, "sessionID")?,
        }),
        "session.error" => Ok(Event::SessionError {
            session_id: require_str(&props, "sessionID")?,
            message: optional_str(&props, "message"),
        }),
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
        "permission.replied" => Ok(Event::PermissionReplied {
            session_id: require_str(&props, "sessionID")?,
            request_id: require_str(&props, "id")?,
        }),
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
// the permission-reply endpoint's request body accepts exactly `once`,
// `always`, or `reject`.
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

/// Build the reply for a decided gate. `session_id`/`request_id` come off
/// the [`PermissionAsk`] that raised it — the relay answers through the same
/// channel that asked, per this adapter's own contract.
// Derivation: OPENCODE-SERVER §Permissions (OpenAPI schema at `GET /doc`) —
// the current permission-reply endpoint: `POST
// /session/{id}/permission/{requestID}/reply`, body `{ "response":
// <decision> }`. The endpoint table on that same page also documents an
// older `POST /session/:id/permissions/:permissionID` (body `{ response,
// remember? }`); this adapter targets only the current shape and treats the
// other as absent, per `docs/PARITY.md`.
pub fn reply_action(
    session_id: &str,
    request_id: &str,
    decision: PermissionDecision,
) -> PermissionReply {
    PermissionReply {
        method: "POST",
        path: format!("/session/{session_id}/permission/{request_id}/reply"),
        body: serde_json::json!({ "response": decision.as_wire_str() }),
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
            r#"{"type":"session.created","properties":{"sessionID":"ses_1","parentID":"ses_0"}}"#,
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
            r#"{"type":"session.error","properties":{"sessionID":"ses_1","message":"boom"}}"#,
        ))
        .expect("parses");
        assert_eq!(
            error,
            Event::SessionError {
                session_id: "ses_1".to_owned(),
                message: Some("boom".to_owned()),
            }
        );

        let deleted = parse_frame(&frame(
            r#"{"type":"session.deleted","properties":{"sessionID":"ses_1"}}"#,
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
        for (wire, expected) in [
            ("idle", SessionStatus::Idle),
            ("busy", SessionStatus::Busy),
            ("retry", SessionStatus::Retry),
        ] {
            let event = parse_frame(&frame(&format!(
                r#"{{"type":"session.status","properties":{{"sessionID":"ses_1","status":"{wire}"}}}}"#
            )))
            .expect("parses");
            assert_eq!(
                event,
                Event::SessionStatusChanged {
                    session_id: "ses_1".to_owned(),
                    status: expected,
                }
            );
        }
    }

    #[test]
    fn permission_asked_becomes_open_gate_with_the_engines_question_and_the_reply_options() {
        let event = parse_frame(&frame(
            r#"{"type":"permission.asked","properties":{"id":"perm_1","sessionID":"ses_1","tool":"bash","patterns":["rm -rf *"]}}"#,
        ))
        .expect("parses");
        let Event::PermissionAsked(ask) = event else {
            panic!("expected PermissionAsked");
        };
        assert_eq!(ask.request_id, "perm_1");
        assert_eq!(ask.session_id, "ses_1");

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
    fn permission_replied_carries_the_request_it_answers() {
        let event = parse_frame(&frame(
            r#"{"type":"permission.replied","properties":{"id":"perm_1","sessionID":"ses_1"}}"#,
        ))
        .expect("parses");
        assert_eq!(
            event,
            Event::PermissionReplied {
                session_id: "ses_1".to_owned(),
                request_id: "perm_1".to_owned(),
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
    fn reply_action_targets_the_current_endpoint_with_the_decided_response() {
        let reply = reply_action("ses_1", "perm_1", PermissionDecision::Once);
        assert_eq!(reply.method, "POST");
        assert_eq!(reply.path, "/session/ses_1/permission/perm_1/reply");
        assert_eq!(reply.body, serde_json::json!({ "response": "once" }));
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
