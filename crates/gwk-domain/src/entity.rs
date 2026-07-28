//! Entity snapshots — the vertical-slice record shapes.
//!
//! These are the wire/storage projections of each aggregate at a version, not
//! the event stream itself. Conventions (see `docs/contract/NAMING.md`):
//! snake_case fields, absent optionals omitted, refs opaque strings, open
//! classification fields are bounded strings (never closed enums), and every
//! CAS-transitioned entity carries `version`.

use std::collections::BTreeMap;

use crate::envelope::Actor;
use crate::fsm::{
    AttemptState, CommandState, GateVerdict, LeaseMode, LeaseState, MessageState, Outcome,
    TaskState,
};
use crate::ids::{
    AttemptId, AttentionItemId, AuthorityGrantId, ByteCount, CommandId, CorrelationId, CostMicros,
    DispatchNodeId, EngineId, EngineSessionId, EvidenceId, FenceToken, GateId, IdempotencyKey,
    LeaseId, MessageId, ReceiptId, TaskId, Timestamp, WorktreeId,
};

/// Tracker-visible work item.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub id: TaskId,
    pub version: u32,
    pub state: TaskState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub spec_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub priority: Option<i32>,
    /// Opaque external-tracker reference (vendor-neutral).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub tracker_ref: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// Resource budget for one attempt. All fields optional: an absent field means
/// "no cap on this axis", never zero.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub max_tool_calls: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub max_wall_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub max_cost_micros: Option<CostMicros>,
}

/// One engine execution of a task, with its full semantic route snapshot.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct Attempt {
    pub id: AttemptId,
    pub version: u32,
    pub state: AttemptState,
    pub task_id: TaskId,
    pub engine: EngineId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub capability: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub role: Option<String>,
    /// The model tier lane (routing), distinct from `engine` (the runtime).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub model_lane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub permission_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub worktree_lease_id: Option<LeaseId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub base_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub budget: Option<Budget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub result_schema_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub provider_session_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub runtime_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub runtime_started_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub provider_terminal_event: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub result_valid: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub evidence_manifest_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub gate_result: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// One provider-level session under an attempt (an attempt may resume across
/// several engine sessions).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct EngineSession {
    pub id: EngineSessionId,
    pub attempt_id: AttemptId,
    pub engine: EngineId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub provider_session_ref: Option<String>,
    pub started_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub ended_at: Option<Timestamp>,
}

/// Governed inter-party message. `delivery_refs` maps a delivery channel name
/// to an opaque per-channel reference (replaces any single-vendor ref column).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub id: MessageId,
    pub version: u32,
    pub state: MessageState,
    pub idempotency_key: IdempotencyKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub correlation_id: Option<CorrelationId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub reply_to: Option<MessageId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub sender: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub recipient: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional, type = Option<crate::envelope::JsonValue>)]
    pub payload: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub deadline: Option<Timestamp>,
    pub delivery_attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub dead_letter_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub delivery_refs: Option<BTreeMap<String, String>>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// A stop/kill command. `outcome` is present iff `state` is
/// `verification_complete` — written in the same transaction as that terminal
/// transition (the invariant `gwk-cert` checks and the DDL CHECKs).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub id: CommandId,
    pub version: u32,
    pub state: CommandState,
    /// OPEN bounded string (e.g. `stop_attempt`).
    pub kind: String,
    // The PRIMARY target of this command: the command event payload carries a
    // `targets` array (a stop may name several attempts); this projection keeps
    // the single primary. Plain `//`, not a `///` doc: a doc attribute here
    // would flow into the generated bindings.ts golden.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub actor: Option<Actor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub idempotency_key: Option<IdempotencyKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub outcome: Option<Outcome>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// A verify/review/security/eval/cert checkpoint. `kind` is OPEN; the verdict
/// set is CLOSED.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct Gate {
    pub id: GateId,
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub attempt_id: Option<AttemptId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub phase_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub kind: Option<String>,
    pub verdict: GateVerdict,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub evidence_ref: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// A standing authorization: what `grantee` may do without paging the operator.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct AuthorityGrant {
    pub id: AuthorityGrantId,
    pub grantee: Actor,
    /// OPEN action class this grant covers (e.g. `deploy`, `auto_answer`).
    pub action_class: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub scope: Option<String>,
    pub granted_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub expires_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub receipt_id: Option<ReceiptId>,
}

/// An attestation that an actor performed an action — the audit row every
/// auto-answer, state flip, and side effect leaves behind. The
/// liveness-producer flip rule's receipt uses `subject_type = "attempt"`,
/// `from`/`to` as the flip edge, and `observed_basis` naming the liveness
/// evidence.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub id: ReceiptId,
    pub actor: Actor,
    /// OPEN action attested (e.g. `state_flip`, `auto_answer`, `deploy`).
    pub action: String,
    pub subject_type: String,
    pub subject_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub observed_basis: Option<String>,
    pub ts: Timestamp,
}

/// A pointer to one piece of evidence. `kind` is an OPEN bounded string
/// (e.g. `transcript`, `diff`, `log`); there is no format column.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub id: EvidenceId,
    pub kind: String,
    pub r#ref: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub byte_size: Option<ByteCount>,
    pub created_at: Timestamp,
}

/// Something the operator should look at. `kind` is OPEN.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct AttentionItem {
    pub id: AttentionItemId,
    pub kind: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub subject_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub raised_by: Option<Actor>,
    pub raised_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub resolved_at: Option<Timestamp>,
}

/// An isolated working copy a writer attempt runs in.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct Worktree {
    pub id: WorktreeId,
    pub repo: String,
    pub path: String,
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub base_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub lease_id: Option<LeaseId>,
    pub dirty: bool,
    pub unpushed: bool,
    pub created_at: Timestamp,
}

/// An advisory lease over a scope (worktree, file set, singleton role).
/// `fence_token` increases on every re-grant of the same scope; storage rejects
/// writes presenting a stale token.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct Lease {
    pub id: LeaseId,
    pub version: u32,
    pub state: LeaseState,
    pub mode: LeaseMode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub holder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub base_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub fence_token: Option<FenceToken>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub heartbeat_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub expires_at: Option<Timestamp>,
    pub dirty: bool,
    pub unpushed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub disposition: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// One node in the dispatch tree (who spawned whom). `kind` is OPEN
/// (e.g. `orchestrator`, `subagent`, `engine`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct DispatchNode {
    pub id: DispatchNodeId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub parent_id: Option<DispatchNodeId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub attempt_id: Option<AttemptId>,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub label: Option<String>,
    pub created_at: Timestamp,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsm::StateMachine;

    #[test]
    fn command_outcome_rides_the_terminal_only() {
        // The invariant is checked by gwk-cert + the DDL; here we pin the shape:
        // outcome is representable exactly as an optional column beside state.
        let cmd = Command {
            id: CommandId::new("cmd-1"),
            version: 4,
            state: CommandState::VerificationComplete,
            kind: "stop_attempt".into(),
            target: None,
            actor: None,
            idempotency_key: None,
            outcome: Some(Outcome::Clean),
            created_at: Timestamp::new("2026-07-27T00:00:00Z"),
            updated_at: Timestamp::new("2026-07-27T00:00:05Z"),
        };
        assert!(cmd.state.is_terminal());
        let json = serde_json::to_string(&cmd).expect("serialize");
        assert!(json.contains("\"outcome\":\"clean\""));
        assert!(json.contains("\"state\":\"verification_complete\""));
    }

    #[test]
    fn message_delivery_refs_is_a_channel_map() {
        let mut refs = BTreeMap::new();
        refs.insert("chat".to_string(), "msg-ref-1".to_string());
        let msg = Message {
            id: MessageId::new("m-1"),
            version: 1,
            state: MessageState::Accepted,
            idempotency_key: IdempotencyKey::new("k-1"),
            correlation_id: None,
            reply_to: None,
            sender: None,
            recipient: None,
            channel: None,
            kind: None,
            payload: None,
            deadline: None,
            delivery_attempts: 0,
            dead_letter_reason: None,
            delivery_refs: Some(refs),
            created_at: Timestamp::new("2026-07-27T00:00:00Z"),
            updated_at: Timestamp::new("2026-07-27T00:00:00Z"),
        };
        let json = serde_json::to_string(&msg).expect("serialize");
        assert!(json.contains("\"delivery_refs\":{\"chat\":\"msg-ref-1\"}"));
        let back: Message = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, msg);
    }
}
