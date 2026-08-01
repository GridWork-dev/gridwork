//! Typed protocol bodies derived from the vendored schemas in `schemas/`.
//!
//! `deny_unknown_fields` is deliberately OFF on every type here: the
//! generator's own bundle documents far more fields per type than this
//! adapter reads (see `schemas/PROVENANCE.md`), and the engine's schema is
//! non_exhaustive by design — new optional fields are additive, not breaking.
//! A struct that only names the fields it uses tolerates the rest for free,
//! because plain `#[derive(Deserialize)]` without `deny_unknown_fields`
//! already ignores unrecognized keys. Every type this crate SENDS (the two
//! approval responses, and their decision enums) is exact instead: nothing
//! here invents a field the schema does not document.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------
// thread/started, thread/status/changed, thread/closed
// ---------------------------------------------------------------------

// Derivation: CODEX-APP-SERVER `schemas/v2/ThreadStartedNotification.json` —
// method `thread/started`, `{ "thread": Thread }`. `Thread` has many more
// required fields than `id`/`status`; the rest are ignored (see module doc).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStartedNotification {
    pub thread: ThreadSummary,
}

/// The slice of `Thread` this adapter reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadSummary {
    pub id: String,
    pub status: ThreadStatus,
    #[serde(default)]
    pub parent_thread_id: Option<String>,
}

// Derivation: CODEX-APP-SERVER `schemas/v2/ThreadStatusChangedNotification.json`
// — method `thread/status/changed`, `{ "threadId": string, "status": ThreadStatus }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadStatusChangedNotification {
    pub thread_id: String,
    pub status: ThreadStatus,
}

// Derivation: CODEX-APP-SERVER `schemas/v2/ThreadClosedNotification.json` —
// method `thread/closed`, `{ "threadId": string }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadClosedNotification {
    pub thread_id: String,
}

// Derivation: CODEX-APP-SERVER `schemas/v2/ThreadStartedNotification.json`
// `#/definitions/ThreadStatus` — a `oneOf` tagged on `type`:
// `notLoaded` | `idle` | `systemError` | `active { activeFlags: [...] }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ThreadStatus {
    NotLoaded,
    Idle,
    SystemError,
    #[serde(rename_all = "camelCase")]
    Active {
        #[serde(default)]
        active_flags: Vec<ThreadActiveFlag>,
    },
}

impl ThreadStatus {
    /// `activeFlags` containing `waitingOnApproval` is first-class waiting
    /// state (`docs/PARITY.md` axis 2), distinct from `active` with no such
    /// flag (working) or with only `waitingOnUserInput`.
    pub fn is_waiting_on_approval(&self) -> bool {
        matches!(
            self,
            ThreadStatus::Active { active_flags }
                if active_flags.contains(&ThreadActiveFlag::WaitingOnApproval)
        )
    }
}

// Derivation: CODEX-APP-SERVER `schemas/v2/ThreadStatusChangedNotification.json`
// `#/definitions/ThreadActiveFlag` — `waitingOnApproval` | `waitingOnUserInput`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ThreadActiveFlag {
    WaitingOnApproval,
    WaitingOnUserInput,
}

// ---------------------------------------------------------------------
// turn/completed, the `error` notification
// ---------------------------------------------------------------------

// Derivation: CODEX-APP-SERVER `schemas/v2/TurnCompletedNotification.json` —
// method `turn/completed`, `{ "threadId": string, "turn": Turn }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnCompletedNotification {
    pub thread_id: String,
    pub turn: Turn,
}

// Derivation: CODEX-APP-SERVER `schemas/v2/TurnCompletedNotification.json`
// `#/definitions/Turn` — `id`, `status`, `error` (populated only when
// `status` is `failed`), and `items` (the `ThreadItem`s this turn payload
// carries; empty unless the caller asked for them).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    pub id: String,
    pub status: TurnStatus,
    #[serde(default)]
    pub error: Option<TurnError>,
    #[serde(default)]
    pub items: Vec<ThreadItem>,
}

// Derivation: CODEX-APP-SERVER `schemas/v2/TurnCompletedNotification.json`
// `#/definitions/TurnStatus` — `completed` | `interrupted` | `failed` | `inProgress`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TurnStatus {
    Completed,
    Interrupted,
    Failed,
    InProgress,
}

// Derivation: CODEX-APP-SERVER `schemas/v2/ErrorNotification.json`
// `#/definitions/TurnError` — `message` required; `additionalDetails` and
// `codexErrorInfo` optional. `codexErrorInfo` is not modeled: this adapter
// relays the message, it does not branch on the vendor's error taxonomy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnError {
    pub message: String,
    #[serde(default)]
    pub additional_details: Option<String>,
}

// Derivation: CODEX-APP-SERVER `schemas/v2/ErrorNotification.json` — the
// `error` notification (bare method name, per `_server_notif_methods.txt`:
// `ErrorNotification -> method: error`), `{ threadId, turnId, error, willRetry }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub error: TurnError,
    pub will_retry: bool,
}

// ---------------------------------------------------------------------
// item/started, item/completed — the typed ThreadItem stream
// ---------------------------------------------------------------------

// Derivation: CODEX-APP-SERVER `schemas/v2/ItemStartedNotification.json` —
// method `item/started`, `{ threadId, turnId, startedAtMs, item }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemStartedNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub started_at_ms: i64,
    pub item: ThreadItem,
}

// Derivation: CODEX-APP-SERVER `schemas/v2/ItemCompletedNotification.json` —
// method `item/completed`, `{ threadId, turnId, completedAtMs, item }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemCompletedNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub completed_at_ms: i64,
    pub item: ThreadItem,
}

/// The subset of `ThreadItem`'s fourteen `type`-tagged variants this adapter
/// reads a shape from. `CollabAgentToolCall` is the one axis 3 names
/// explicitly (`docs/PARITY.md`: "per-child status explicit on the parent's
/// transcript (`collabAgentToolCall` → `agentsStates`)") — every other kind
/// this adapter has no reason to inspect (plan, webSearch, imageView, sleep,
/// imageGeneration, the review-mode markers, contextCompaction,
/// hookPrompt, dynamicToolCall) falls into `Other` via `#[serde(other)]`
/// rather than failing to decode: the item stream must survive a variant
/// this adapter has not been taught yet, the same non_exhaustive posture as
/// every other type in this module.
// Derivation: CODEX-APP-SERVER `schemas/v2/ItemCompletedNotification.json`
// `#/definitions/ThreadItem` — internally tagged on `type`, one variant per
// item kind. Variant tag strings and field names below are quoted from that
// definition (e.g. `userMessage`, `agentMessage`, `commandExecution`,
// `fileChange`, `mcpToolCall`, `collabAgentToolCall`, `subAgentActivity`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ThreadItem {
    UserMessage {
        id: String,
    },
    AgentMessage {
        id: String,
        text: String,
    },
    Reasoning {
        id: String,
    },
    CommandExecution {
        id: String,
        command: String,
        status: CommandExecutionStatus,
    },
    FileChange {
        id: String,
        status: PatchApplyStatus,
    },
    McpToolCall {
        id: String,
        server: String,
        tool: String,
        status: McpToolCallStatus,
    },
    #[serde(rename_all = "camelCase")]
    CollabAgentToolCall {
        id: String,
        sender_thread_id: String,
        #[serde(default)]
        receiver_thread_ids: Vec<String>,
        #[serde(default)]
        agents_states: BTreeMap<String, CollabAgentState>,
        status: CollabAgentToolCallStatus,
    },
    #[serde(rename_all = "camelCase")]
    SubAgentActivity {
        id: String,
        agent_thread_id: String,
        kind: SubAgentActivityKind,
    },
    #[serde(other)]
    Other,
}

impl ThreadItem {
    /// The item's own `id`, when this variant carries one — every modeled
    /// variant does; only the `Other` catch-all does not.
    pub fn id(&self) -> Option<&str> {
        match self {
            ThreadItem::UserMessage { id }
            | ThreadItem::AgentMessage { id, .. }
            | ThreadItem::Reasoning { id }
            | ThreadItem::CommandExecution { id, .. }
            | ThreadItem::FileChange { id, .. }
            | ThreadItem::McpToolCall { id, .. }
            | ThreadItem::CollabAgentToolCall { id, .. }
            | ThreadItem::SubAgentActivity { id, .. } => Some(id.as_str()),
            ThreadItem::Other => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CommandExecutionStatus {
    InProgress,
    Completed,
    Failed,
    Declined,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PatchApplyStatus {
    InProgress,
    Completed,
    Failed,
    Declined,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum McpToolCallStatus {
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CollabAgentToolCallStatus {
    InProgress,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SubAgentActivityKind {
    Started,
    Interacted,
    Interrupted,
}

// Derivation: CODEX-APP-SERVER `schemas/v2/ItemCompletedNotification.json`
// `#/definitions/CollabAgentState` — `{ status: CollabAgentStatus, message?: string }`,
// the per-child status map's value type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CollabAgentState {
    pub status: CollabAgentStatus,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CollabAgentStatus {
    PendingInit,
    Running,
    Interrupted,
    Completed,
    Errored,
    Shutdown,
    NotFound,
}

// ---------------------------------------------------------------------
// thread/tokenUsage/updated
// ---------------------------------------------------------------------

// Derivation: CODEX-APP-SERVER
// `schemas/v2/ThreadTokenUsageUpdatedNotification.json` — method
// `thread/tokenUsage/updated`, `{ threadId, turnId, tokenUsage }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadTokenUsageUpdatedNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub token_usage: ThreadTokenUsage,
}

// Derivation: CODEX-APP-SERVER
// `schemas/v2/ThreadTokenUsageUpdatedNotification.json`
// `#/definitions/ThreadTokenUsage` — `last` (this notification's own report)
// and `total` (the thread's running cumulative figure) are both required;
// `modelContextWindow` is optional.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadTokenUsage {
    pub last: TokenUsageBreakdown,
    pub total: TokenUsageBreakdown,
    #[serde(default)]
    pub model_context_window: Option<i64>,
}

// Derivation: CODEX-APP-SERVER
// `schemas/v2/ThreadTokenUsageUpdatedNotification.json`
// `#/definitions/TokenUsageBreakdown` — five required int64 counts plus
// `cacheWriteInputTokens`, which the schema defaults to 0 rather than
// requiring. No currency field exists anywhere on this type or its parent
// (`docs/PARITY.md` axis 3: "no currency exists anywhere in the protocol").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsageBreakdown {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    #[serde(default)]
    pub cache_write_input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_output_tokens: i64,
    pub total_tokens: i64,
}

// ---------------------------------------------------------------------
// serverRequest/resolved
// ---------------------------------------------------------------------

// Derivation: CODEX-APP-SERVER
// `schemas/v2/ServerRequestResolvedNotification.json` — method
// `serverRequest/resolved`, `{ requestId: RequestId, threadId: string }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerRequestResolvedNotification {
    pub request_id: crate::wire::JsonRpcId,
    pub thread_id: String,
}

// ---------------------------------------------------------------------
// item/commandExecution/requestApproval, item/fileChange/requestApproval
// ---------------------------------------------------------------------

// Derivation: CODEX-APP-SERVER
// `schemas/CommandExecutionRequestApprovalParams.json` — the params of the
// server-initiated request `item/commandExecution/requestApproval`.
// `command`, `reason`, and `availableDecisions` are all nullable on the
// wire; every other documented field (`commandActions`, `cwd`,
// `environmentId`, `networkApprovalContext`, the amendment-proposal fields)
// is not modeled here — see `command.rs` for why the question this adapter
// asks needs only these three.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandExecutionRequestApprovalParams {
    pub item_id: String,
    pub thread_id: String,
    pub turn_id: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub available_decisions: Option<Vec<CommandExecutionApprovalDecision>>,
}

// Derivation: CODEX-APP-SERVER
// `schemas/CommandExecutionRequestApprovalResponse.json` — the JSON-RPC
// result this adapter's response to that request must carry:
// `{ "decision": CommandExecutionApprovalDecision }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandExecutionRequestApprovalResponse {
    pub decision: CommandExecutionApprovalDecision,
}

// Derivation: CODEX-APP-SERVER
// `schemas/CommandExecutionRequestApprovalParams.json`
// `#/definitions/CommandExecutionApprovalDecision` — a `oneOf` mixing four
// bare-string variants with two single-key-object variants. Rust/serde's
// DEFAULT (externally tagged) enum representation already produces exactly
// that shape with no `#[serde(tag = ...)]` needed: a unit variant encodes as
// its (renamed) name, a struct variant as `{ "name": { fields } }`. The two
// structured variants' inner field names (`execpolicy_amendment`,
// `network_policy_amendment`) are quoted snake_case from the schema — the
// vendor did not camelCase them, so `#[serde(rename_all = "camelCase")]`
// is deliberately NOT applied inside those two variants; only the variant
// tag names (via the enum-level attribute) are camelCase.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CommandExecutionApprovalDecision {
    Accept,
    AcceptForSession,
    AcceptWithExecpolicyAmendment {
        execpolicy_amendment: Vec<String>,
    },
    ApplyNetworkPolicyAmendment {
        network_policy_amendment: NetworkPolicyAmendment,
    },
    Decline,
    Cancel,
}

impl CommandExecutionApprovalDecision {
    /// The four decisions a bare option tag round-trips through cleanly —
    /// the two amendment variants carry data an option string cannot, so
    /// they are never offered as a `chosen_option` (see `command.rs`).
    pub const PLAIN: [CommandExecutionApprovalDecision; 4] = [
        CommandExecutionApprovalDecision::Accept,
        CommandExecutionApprovalDecision::AcceptForSession,
        CommandExecutionApprovalDecision::Decline,
        CommandExecutionApprovalDecision::Cancel,
    ];

    /// The wire tag this decision serializes as — `"accept"`,
    /// `"acceptForSession"`, `"decline"`, or `"cancel"` for the plain
    /// variants; `None` for the two amendment variants, which are never
    /// offered as a bare option (see `PLAIN`).
    pub fn wire_tag(&self) -> Option<&'static str> {
        match self {
            Self::Accept => Some("accept"),
            Self::AcceptForSession => Some("acceptForSession"),
            Self::Decline => Some("decline"),
            Self::Cancel => Some("cancel"),
            Self::AcceptWithExecpolicyAmendment { .. }
            | Self::ApplyNetworkPolicyAmendment { .. } => None,
        }
    }

    /// The inverse of `wire_tag` over `PLAIN`.
    pub fn from_wire_tag(tag: &str) -> Option<Self> {
        Self::PLAIN.into_iter().find(|d| d.wire_tag() == Some(tag))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkPolicyAmendment {
    pub action: NetworkPolicyRuleAction,
    pub host: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NetworkPolicyRuleAction {
    Allow,
    Deny,
}

// Derivation: CODEX-APP-SERVER `schemas/FileChangeRequestApprovalParams.json`
// — the params of the server-initiated request
// `item/fileChange/requestApproval`. `grantRoot` and `reason` are nullable;
// unlike command execution, this request carries no `availableDecisions` —
// the offered set is always the fixed four-variant `FileChangeApprovalDecision`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChangeRequestApprovalParams {
    pub item_id: String,
    pub thread_id: String,
    pub turn_id: String,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub grant_root: Option<String>,
}

// Derivation: CODEX-APP-SERVER
// `schemas/FileChangeRequestApprovalResponse.json` — `{ "decision": FileChangeApprovalDecision }`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChangeRequestApprovalResponse {
    pub decision: FileChangeApprovalDecision,
}

// Derivation: CODEX-APP-SERVER
// `schemas/FileChangeRequestApprovalResponse.json`
// `#/definitions/FileChangeApprovalDecision` — four bare-string variants,
// no structured ones (unlike the command-execution decision above).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum FileChangeApprovalDecision {
    Accept,
    AcceptForSession,
    Decline,
    Cancel,
}

impl FileChangeApprovalDecision {
    pub const ALL: [FileChangeApprovalDecision; 4] = [
        Self::Accept,
        Self::AcceptForSession,
        Self::Decline,
        Self::Cancel,
    ];

    pub fn wire_tag(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::AcceptForSession => "acceptForSession",
            Self::Decline => "decline",
            Self::Cancel => "cancel",
        }
    }

    pub fn from_wire_tag(tag: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|d| d.wire_tag() == tag)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_status_active_with_waiting_on_approval_is_waiting() {
        let json = serde_json::json!({"type": "active", "activeFlags": ["waitingOnApproval"]});
        let status: ThreadStatus = serde_json::from_value(json).expect("decodes");
        assert!(status.is_waiting_on_approval());
    }

    #[test]
    fn thread_status_active_without_the_flag_is_not_waiting() {
        let json = serde_json::json!({"type": "active", "activeFlags": ["waitingOnUserInput"]});
        let status: ThreadStatus = serde_json::from_value(json).expect("decodes");
        assert!(!status.is_waiting_on_approval());

        let idle: ThreadStatus =
            serde_json::from_value(serde_json::json!({"type": "idle"})).expect("decodes");
        assert!(!idle.is_waiting_on_approval());
    }

    #[test]
    fn thread_started_ignores_fields_it_does_not_model() {
        // Real `Thread` objects carry a dozen more required fields
        // (cliVersion, createdAt, cwd, ...) than this type names; the point
        // of `deny_unknown_fields` staying off is that they are tolerated.
        let json = serde_json::json!({
            "thread": {
                "id": "th-1",
                "status": {"type": "idle"},
                "cliVersion": "0.146.0",
                "createdAt": 0,
                "cwd": "/repo",
                "ephemeral": false,
                "modelProvider": "openai",
                "preview": "",
                "sessionId": "sess-1",
                "source": "cli",
                "turns": [],
                "updatedAt": 0
            }
        });
        let decoded: ThreadStartedNotification = serde_json::from_value(json).expect("decodes");
        assert_eq!(decoded.thread.id, "th-1");
        assert_eq!(decoded.thread.status, ThreadStatus::Idle);
        assert_eq!(decoded.thread.parent_thread_id, None);
    }

    #[test]
    fn thread_item_collab_agent_tool_call_carries_the_per_child_state_map() {
        let json = serde_json::json!({
            "type": "collabAgentToolCall",
            "id": "call-1",
            "senderThreadId": "th-parent",
            "receiverThreadIds": ["th-child-1", "th-child-2"],
            "agentsStates": {
                "th-child-1": {"status": "running"},
                "th-child-2": {"status": "completed", "message": "done"}
            },
            "status": "inProgress",
            "tool": "spawnAgent"
        });
        let item: ThreadItem = serde_json::from_value(json).expect("decodes");
        match item {
            ThreadItem::CollabAgentToolCall {
                id,
                sender_thread_id,
                receiver_thread_ids,
                agents_states,
                status,
            } => {
                assert_eq!(id, "call-1");
                assert_eq!(sender_thread_id, "th-parent");
                assert_eq!(receiver_thread_ids, vec!["th-child-1", "th-child-2"]);
                assert_eq!(agents_states.len(), 2);
                assert_eq!(agents_states["th-child-2"].message.as_deref(), Some("done"));
                assert_eq!(status, CollabAgentToolCallStatus::InProgress);
            }
            other => panic!("expected CollabAgentToolCall, got {other:?}"),
        }
    }

    #[test]
    fn thread_item_unmodeled_variant_falls_into_other_instead_of_failing() {
        let json = serde_json::json!({"type": "plan", "id": "p-1", "text": "do the thing"});
        let item: ThreadItem = serde_json::from_value(json).expect("decodes as Other");
        assert_eq!(item, ThreadItem::Other);
        assert_eq!(item.id(), None);
    }

    #[test]
    fn command_execution_decision_plain_variants_round_trip_through_a_wire_tag() {
        for decision in CommandExecutionApprovalDecision::PLAIN {
            let tag = decision.wire_tag().expect("plain variant has a tag");
            assert_eq!(
                CommandExecutionApprovalDecision::from_wire_tag(tag).as_ref(),
                Some(&decision)
            );
        }
    }

    #[test]
    fn command_execution_decision_matches_the_schemas_exact_wire_shape() {
        let accept =
            serde_json::to_value(CommandExecutionApprovalDecision::Accept).expect("serializes");
        assert_eq!(accept, serde_json::json!("accept"));

        let amended = CommandExecutionApprovalDecision::AcceptWithExecpolicyAmendment {
            execpolicy_amendment: vec!["allow rm -rf /tmp/*".to_owned()],
        };
        let json = serde_json::to_value(&amended).expect("serializes");
        assert_eq!(
            json,
            serde_json::json!({
                "acceptWithExecpolicyAmendment": {
                    "execpolicy_amendment": ["allow rm -rf /tmp/*"]
                }
            })
        );
        assert_eq!(amended.wire_tag(), None);
    }

    #[test]
    fn file_change_decision_round_trips_through_a_wire_tag() {
        for decision in FileChangeApprovalDecision::ALL {
            assert_eq!(
                FileChangeApprovalDecision::from_wire_tag(decision.wire_tag()),
                Some(decision)
            );
        }
        assert_eq!(FileChangeApprovalDecision::from_wire_tag("bogus"), None);
    }

    #[test]
    fn token_usage_breakdown_defaults_cache_write_tokens_when_absent() {
        let json = serde_json::json!({
            "inputTokens": 100,
            "cachedInputTokens": 10,
            "outputTokens": 20,
            "reasoningOutputTokens": 5,
            "totalTokens": 130
        });
        let breakdown: TokenUsageBreakdown = serde_json::from_value(json).expect("decodes");
        assert_eq!(breakdown.cache_write_input_tokens, 0);
    }
}
