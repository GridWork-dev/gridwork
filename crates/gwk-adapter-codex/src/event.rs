//! The normalized event stream: lifecycle, status, items, and approval asks,
//! independent of the JSON-RPC frame shape that carried them.
//!
//! [`normalize_notification`] and [`normalize_request`] are the two entry
//! points a caller pumping [`crate::wire::WireClient`] drives: every
//! [`crate::wire::NotificationFrame`] goes through the first, every
//! [`crate::wire::RequestFrame`] through the second. Method names not this
//! adapter's business (the dozens of other notifications and requests the
//! app-server protocol carries — MCP OAuth, plugin management, remote
//! control, and so on) surface as `Unrecognized`/`Unsupported` rather than
//! an error: an adapter that panicked or refused to progress on a method it
//! has not been taught yet would make every protocol extension a breaking
//! change for this crate, which is exactly the non_exhaustive posture
//! `schema.rs`'s module doc already commits to for individual fields.

use crate::schema::{
    CommandExecutionRequestApprovalParams, ErrorNotification, FileChangeRequestApprovalParams,
    ItemCompletedNotification, ItemStartedNotification, ServerRequestResolvedNotification,
    ThreadClosedNotification, ThreadItem, ThreadStartedNotification, ThreadStatus,
    ThreadStatusChangedNotification, ThreadTokenUsage, ThreadTokenUsageUpdatedNotification,
    TurnCompletedNotification, TurnError,
};
use crate::wire::JsonRpcId;

// Derivation: CODEX-APP-SERVER `schemas/ServerNotification.json` — its
// `oneOf` pairs each notification's `method` string enum with the
// `#/definitions/...` its `params` `$ref`s (e.g. the `ErrorNotification`
// branch: `method: ["error"]`, `params: $ref ErrorNotification`; the
// `Thread/startedNotification` branch: `method: ["thread/started"]`,
// `params: $ref ThreadStartedNotification`). Quoted here because
// `schema.rs`'s types decode a notification's `params`; nothing in that
// module names the method string itself.
pub(crate) const METHOD_THREAD_STARTED: &str = "thread/started";
const METHOD_THREAD_STATUS_CHANGED: &str = "thread/status/changed";
const METHOD_THREAD_CLOSED: &str = "thread/closed";
const METHOD_TURN_COMPLETED: &str = "turn/completed";
const METHOD_ERROR: &str = "error";
pub(crate) const METHOD_ITEM_STARTED: &str = "item/started";
pub(crate) const METHOD_ITEM_COMPLETED: &str = "item/completed";
const METHOD_TOKEN_USAGE_UPDATED: &str = "thread/tokenUsage/updated";
pub(crate) const METHOD_SERVER_REQUEST_RESOLVED: &str = "serverRequest/resolved";

// Derivation: CODEX-APP-SERVER `schemas/ServerRequest.json` — the
// `Item/commandExecution/requestApprovalRequest` and
// `Item/fileChange/requestApprovalRequest` branches' `method` enum values,
// the two approval-relay request kinds this adapter relays.
const METHOD_COMMAND_EXECUTION_REQUEST_APPROVAL: &str = "item/commandExecution/requestApproval";
const METHOD_FILE_CHANGE_REQUEST_APPROVAL: &str = "item/fileChange/requestApproval";

/// One normalized fact this adapter reports, independent of which JSON-RPC
/// shape (request vs. notification) carried it on the wire.
#[derive(Debug, Clone, PartialEq)]
pub enum CodexEvent {
    /// `thread/started` — lifecycle: start.
    ThreadStarted {
        thread_id: String,
        status: ThreadStatus,
    },
    /// `thread/status/changed` — status truth, including the
    /// `waitingOnApproval` active flag.
    StatusChanged {
        thread_id: String,
        status: ThreadStatus,
    },
    /// `thread/closed` — lifecycle: end.
    ThreadClosed { thread_id: String },
    /// `turn/completed` — the typed item batch for this turn, and the error
    /// half of `docs/PARITY.md` axis 1 when it carries one.
    ///
    /// NOT idle. This said "lifecycle: idle (on `TurnStatus::Completed`)" and
    /// that was never the contract: axis 1's codex row lists this notification
    /// under ERROR alone, assigns idle to `thread/status/changed` and end to
    /// `thread/closed`. `crate::adapter` reads it the row's way; this doc is
    /// where the misreading started.
    TurnCompleted {
        thread_id: String,
        turn: crate::schema::Turn,
    },
    /// The `error` notification — lifecycle: error.
    TurnError {
        thread_id: String,
        turn_id: String,
        error: TurnError,
        will_retry: bool,
    },
    /// `item/started` — transcript ingestion.
    ItemStarted {
        thread_id: String,
        turn_id: String,
        item: ThreadItem,
    },
    /// `item/completed` — transcript ingestion.
    ItemCompleted {
        thread_id: String,
        turn_id: String,
        item: ThreadItem,
    },
    /// `thread/tokenUsage/updated` — the `RecordCostEntry` source.
    TokenUsageUpdated {
        thread_id: String,
        turn_id: String,
        usage: ThreadTokenUsage,
    },
    /// `serverRequest/resolved` — approval-relay clearance confirmed.
    ApprovalResolved {
        request_id: JsonRpcId,
        thread_id: String,
    },
    /// A notification method this adapter does not model. Carried rather
    /// than dropped, so a caller can at least log what it skipped.
    Unrecognized { method: String },
}

/// A server-initiated approval request this adapter relays, plus the
/// `id` the eventual response must echo.
#[derive(Debug, Clone, PartialEq)]
pub enum ApprovalRequest {
    CommandExecution {
        id: JsonRpcId,
        params: CommandExecutionRequestApprovalParams,
    },
    FileChange {
        id: JsonRpcId,
        params: FileChangeRequestApprovalParams,
    },
}

impl ApprovalRequest {
    pub fn id(&self) -> &JsonRpcId {
        match self {
            ApprovalRequest::CommandExecution { id, .. } => id,
            ApprovalRequest::FileChange { id, .. } => id,
        }
    }
}

/// Why a notification or request's `params` could not be normalized.
#[derive(Debug, thiserror::Error)]
pub enum NormalizeError {
    #[error("{method} params did not match the expected shape: {source}")]
    Malformed {
        method: String,
        #[source]
        source: serde_json::Error,
    },
    /// A server-initiated request whose method this adapter does not relay.
    /// Distinct from `Unrecognized` notifications: a request demands a
    /// JSON-RPC reply (see [`crate::wire::Frame::error`]), so silently
    /// dropping it would leave the engine's caller waiting forever.
    #[error("{method} is not a request this adapter relays")]
    UnsupportedRequest { method: String },
}

fn decode<T: serde::de::DeserializeOwned>(
    method: &str,
    params: Option<serde_json::Value>,
) -> Result<T, NormalizeError> {
    let params = params.unwrap_or(serde_json::Value::Null);
    serde_json::from_value(params).map_err(|source| NormalizeError::Malformed {
        method: method.to_owned(),
        source,
    })
}

/// Normalize one `method` + `params` pair from a [`crate::wire::NotificationFrame`].
pub fn normalize_notification(
    method: &str,
    params: Option<serde_json::Value>,
) -> Result<CodexEvent, NormalizeError> {
    Ok(match method {
        METHOD_THREAD_STARTED => {
            let n: ThreadStartedNotification = decode(method, params)?;
            CodexEvent::ThreadStarted {
                thread_id: n.thread.id,
                status: n.thread.status,
            }
        }
        METHOD_THREAD_STATUS_CHANGED => {
            let n: ThreadStatusChangedNotification = decode(method, params)?;
            CodexEvent::StatusChanged {
                thread_id: n.thread_id,
                status: n.status,
            }
        }
        METHOD_THREAD_CLOSED => {
            let n: ThreadClosedNotification = decode(method, params)?;
            CodexEvent::ThreadClosed {
                thread_id: n.thread_id,
            }
        }
        METHOD_TURN_COMPLETED => {
            let n: TurnCompletedNotification = decode(method, params)?;
            CodexEvent::TurnCompleted {
                thread_id: n.thread_id,
                turn: n.turn,
            }
        }
        METHOD_ERROR => {
            let n: ErrorNotification = decode(method, params)?;
            CodexEvent::TurnError {
                thread_id: n.thread_id,
                turn_id: n.turn_id,
                error: n.error,
                will_retry: n.will_retry,
            }
        }
        METHOD_ITEM_STARTED => {
            let n: ItemStartedNotification = decode(method, params)?;
            CodexEvent::ItemStarted {
                thread_id: n.thread_id,
                turn_id: n.turn_id,
                item: n.item,
            }
        }
        METHOD_ITEM_COMPLETED => {
            let n: ItemCompletedNotification = decode(method, params)?;
            CodexEvent::ItemCompleted {
                thread_id: n.thread_id,
                turn_id: n.turn_id,
                item: n.item,
            }
        }
        METHOD_TOKEN_USAGE_UPDATED => {
            let n: ThreadTokenUsageUpdatedNotification = decode(method, params)?;
            CodexEvent::TokenUsageUpdated {
                thread_id: n.thread_id,
                turn_id: n.turn_id,
                usage: n.token_usage,
            }
        }
        METHOD_SERVER_REQUEST_RESOLVED => {
            let n: ServerRequestResolvedNotification = decode(method, params)?;
            CodexEvent::ApprovalResolved {
                request_id: n.request_id,
                thread_id: n.thread_id,
            }
        }
        other => CodexEvent::Unrecognized {
            method: other.to_owned(),
        },
    })
}

/// Normalize one `id` + `method` + `params` triple from a
/// [`crate::wire::RequestFrame`] into an [`ApprovalRequest`] this adapter
/// relays. Any other method is `Err(UnsupportedRequest)` — the caller is
/// expected to answer it with [`crate::wire::Frame::error`] so the engine's
/// own caller does not hang waiting on a reply that will never come.
pub fn normalize_request(
    id: JsonRpcId,
    method: &str,
    params: Option<serde_json::Value>,
) -> Result<ApprovalRequest, NormalizeError> {
    match method {
        METHOD_COMMAND_EXECUTION_REQUEST_APPROVAL => Ok(ApprovalRequest::CommandExecution {
            id,
            params: decode(method, params)?,
        }),
        METHOD_FILE_CHANGE_REQUEST_APPROVAL => Ok(ApprovalRequest::FileChange {
            id,
            params: decode(method, params)?,
        }),
        other => Err(NormalizeError::UnsupportedRequest {
            method: other.to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{CollabAgentToolCallStatus, TurnStatus};

    #[test]
    fn thread_started_normalizes_the_id_and_status() {
        let params = serde_json::json!({
            "thread": {"id": "th-1", "status": {"type": "idle"}}
        });
        let event =
            normalize_notification(METHOD_THREAD_STARTED, Some(params)).expect("normalizes");
        assert_eq!(
            event,
            CodexEvent::ThreadStarted {
                thread_id: "th-1".to_owned(),
                status: ThreadStatus::Idle,
            }
        );
    }

    #[test]
    fn status_changed_surfaces_waiting_on_approval() {
        let params = serde_json::json!({
            "threadId": "th-1",
            "status": {"type": "active", "activeFlags": ["waitingOnApproval"]}
        });
        let event =
            normalize_notification(METHOD_THREAD_STATUS_CHANGED, Some(params)).expect("normalizes");
        let CodexEvent::StatusChanged { status, .. } = event else {
            panic!("expected StatusChanged");
        };
        assert!(status.is_waiting_on_approval());
    }

    #[test]
    fn thread_closed_and_turn_completed_normalize() {
        let closed = normalize_notification(
            METHOD_THREAD_CLOSED,
            Some(serde_json::json!({"threadId": "th-1"})),
        )
        .expect("normalizes");
        assert_eq!(
            closed,
            CodexEvent::ThreadClosed {
                thread_id: "th-1".to_owned()
            }
        );

        let turn = normalize_notification(
            METHOD_TURN_COMPLETED,
            Some(serde_json::json!({
                "threadId": "th-1",
                "turn": {"id": "turn-1", "status": "completed", "items": []}
            })),
        )
        .expect("normalizes");
        let CodexEvent::TurnCompleted { thread_id, turn } = turn else {
            panic!("expected TurnCompleted");
        };
        assert_eq!(thread_id, "th-1");
        assert_eq!(turn.status, TurnStatus::Completed);
    }

    #[test]
    fn the_error_notification_carries_will_retry() {
        let event = normalize_notification(
            METHOD_ERROR,
            Some(serde_json::json!({
                "threadId": "th-1",
                "turnId": "turn-1",
                "error": {"message": "rate limited"},
                "willRetry": true
            })),
        )
        .expect("normalizes");
        assert_eq!(
            event,
            CodexEvent::TurnError {
                thread_id: "th-1".to_owned(),
                turn_id: "turn-1".to_owned(),
                error: TurnError {
                    message: "rate limited".to_owned(),
                    additional_details: None,
                },
                will_retry: true,
            }
        );
    }

    #[test]
    fn item_started_and_completed_carry_the_typed_item() {
        let item_json = serde_json::json!({
            "type": "collabAgentToolCall",
            "id": "call-1",
            "senderThreadId": "th-parent",
            "receiverThreadIds": ["th-child"],
            "agentsStates": {"th-child": {"status": "running"}},
            "status": "inProgress",
            "tool": "spawnAgent"
        });

        let started = normalize_notification(
            METHOD_ITEM_STARTED,
            Some(serde_json::json!({
                "threadId": "th-parent", "turnId": "turn-1",
                "startedAtMs": 0, "item": item_json.clone()
            })),
        )
        .expect("normalizes");
        let CodexEvent::ItemStarted { item, .. } = started else {
            panic!("expected ItemStarted");
        };
        assert!(matches!(
            item,
            ThreadItem::CollabAgentToolCall {
                status: CollabAgentToolCallStatus::InProgress,
                ..
            }
        ));

        let completed = normalize_notification(
            METHOD_ITEM_COMPLETED,
            Some(serde_json::json!({
                "threadId": "th-parent", "turnId": "turn-1",
                "completedAtMs": 1, "item": item_json
            })),
        )
        .expect("normalizes");
        assert!(matches!(completed, CodexEvent::ItemCompleted { .. }));
    }

    #[test]
    fn token_usage_updated_normalizes() {
        let params = serde_json::json!({
            "threadId": "th-1",
            "turnId": "turn-1",
            "tokenUsage": {
                "last": {
                    "inputTokens": 100, "cachedInputTokens": 0,
                    "outputTokens": 20, "reasoningOutputTokens": 0, "totalTokens": 120
                },
                "total": {
                    "inputTokens": 500, "cachedInputTokens": 0,
                    "outputTokens": 80, "reasoningOutputTokens": 0, "totalTokens": 580
                }
            }
        });
        let event =
            normalize_notification(METHOD_TOKEN_USAGE_UPDATED, Some(params)).expect("normalizes");
        let CodexEvent::TokenUsageUpdated { usage, .. } = event else {
            panic!("expected TokenUsageUpdated");
        };
        assert_eq!(usage.last.total_tokens, 120);
        assert_eq!(usage.total.total_tokens, 580);
    }

    #[test]
    fn server_request_resolved_normalizes() {
        let event = normalize_notification(
            METHOD_SERVER_REQUEST_RESOLVED,
            Some(serde_json::json!({"requestId": "req-1", "threadId": "th-1"})),
        )
        .expect("normalizes");
        assert_eq!(
            event,
            CodexEvent::ApprovalResolved {
                request_id: JsonRpcId::Str("req-1".to_owned()),
                thread_id: "th-1".to_owned(),
            }
        );
    }

    #[test]
    fn an_unmodeled_notification_method_is_unrecognized_not_an_error() {
        let event = normalize_notification("account/updated", Some(serde_json::json!({})))
            .expect("does not error");
        assert_eq!(
            event,
            CodexEvent::Unrecognized {
                method: "account/updated".to_owned()
            }
        );
    }

    #[test]
    fn malformed_params_is_a_typed_error() {
        let err = normalize_notification(
            METHOD_THREAD_STARTED,
            Some(serde_json::json!({"thread": "not an object"})),
        )
        .expect_err("thread must be an object");
        assert!(matches!(err, NormalizeError::Malformed { .. }));
    }

    #[test]
    fn command_execution_and_file_change_requests_normalize() {
        let command = normalize_request(
            JsonRpcId::Str("req-1".to_owned()),
            METHOD_COMMAND_EXECUTION_REQUEST_APPROVAL,
            Some(serde_json::json!({
                "itemId": "item-1", "threadId": "th-1", "turnId": "turn-1",
                "command": "rm -rf /tmp/x"
            })),
        )
        .expect("normalizes");
        assert!(matches!(command, ApprovalRequest::CommandExecution { .. }));
        assert_eq!(command.id(), &JsonRpcId::Str("req-1".to_owned()));

        let file_change = normalize_request(
            JsonRpcId::Num(2),
            METHOD_FILE_CHANGE_REQUEST_APPROVAL,
            Some(serde_json::json!({
                "itemId": "item-2", "threadId": "th-1", "turnId": "turn-1"
            })),
        )
        .expect("normalizes");
        assert!(matches!(file_change, ApprovalRequest::FileChange { .. }));
    }

    #[test]
    fn an_unsupported_request_method_is_a_typed_error() {
        let err = normalize_request(JsonRpcId::Num(1), "currentTime/read", None)
            .expect_err("not a relayed method");
        assert!(matches!(err, NormalizeError::UnsupportedRequest { .. }));
    }
}
