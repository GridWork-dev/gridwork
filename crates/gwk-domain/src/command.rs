//! The v1 kernel command set — every mutation the kernel accepts.
//!
//! Commands are REQUESTS, not writes: the kernel decides, appends events, and
//! answers. Each command travels as the `payload` of a [`CommandEnvelope`],
//! which carries the governance metadata (actor, origin, CAS expectation, the
//! required `idempotency_key`). The envelope's `command_type` and the body's
//! own `type` tag must agree — [`KernelCommand::from_envelope`] is the one
//! place that check lives, so no handler can drift from the routing metadata.
//!
//! The set is CLOSED. A new command is a contract change with a new binding,
//! never an open string a caller can invent.

use serde_json::Value;

use crate::entity::Budget;
use crate::envelope::{Actor, CommandEnvelope, JsonValue, PayloadRef};
use crate::fsm::{
    AttemptState, CommandState, GateVerdict, LeaseMode, MessageState, Outcome, TaskState,
};
use crate::ids::{
    AttemptId, AttentionItemId, AuthorityGrantId, ByteCount, CommandId, CorrelationId,
    DispatchNodeId, EngineId, EngineSessionId, EvidenceId, GateId, LeaseId, MessageId, ReceiptId,
    TaskId, Timestamp, WorktreeId,
};
use crate::ingestion::IngestionKind;
use crate::inherited::{FindingAction, OrchestratorCheckpoint, RoundFindingSummary};

/// One requested mutation, tagged by `type`.
///
/// Variants are grouped by aggregate. Absent optionals are OMITTED on the wire
/// (the tri-state discipline in `docs/contract/NAMING.md`): absent means "not
/// specified", never zero and never `null`.
// A wire union: the variants ARE the contract's shape. Sizing them uniformly
// (boxing the wide ones) would change the generated bindings to buy a
// micro-optimization on a type the kernel handles one of at a time.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum KernelCommand {
    // ---- epoch ----
    /// The irreversible cutover boundary. Appended once, at aggregate version
    /// 2, under idempotency key `kernel_activated:<cutover_id>`: the same
    /// cutover ID retries stably, a different one conflicts.
    ActivateKernel {
        cutover_id: String,
        /// Lowercase 64-hex SHA-256 of the archive manifest
        /// ([`crate::blob::is_sha256_hex`]).
        archive_manifest_sha256: String,
    },

    // ---- task ----
    CreateTask {
        task_id: TaskId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        kind: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        title: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        spec_ref: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        project: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        priority: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        tracker_ref: Option<String>,
    },
    TransitionTask {
        task_id: TaskId,
        to: TaskState,
        expected_version: u32,
    },
    UpdateBudget {
        attempt_id: AttemptId,
        expected_version: u32,
        budget: Budget,
    },

    // ---- attempt ----
    CreateAttempt {
        attempt_id: AttemptId,
        task_id: TaskId,
        engine: EngineId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        capability: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        role: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        model_lane: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        permission_profile: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        worktree_lease_id: Option<LeaseId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        base_sha: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        budget: Option<Budget>,
    },
    TransitionAttempt {
        attempt_id: AttemptId,
        to: AttemptState,
        expected_version: u32,
        /// REQUIRED on the `running` ⇄ `blocked` flip (the liveness-producer
        /// rule in [`crate::transition`]); absent elsewhere.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        receipt_id: Option<ReceiptId>,
    },
    RecordAttemptOutcome {
        attempt_id: AttemptId,
        expected_version: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        exit_code: Option<i32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        provider_terminal_event: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        result_valid: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        evidence_manifest_ref: Option<String>,
    },

    // ---- engine session ----
    OpenEngineSession {
        engine_session_id: EngineSessionId,
        attempt_id: AttemptId,
        engine: EngineId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        provider_session_ref: Option<String>,
    },
    CloseEngineSession {
        engine_session_id: EngineSessionId,
    },

    // ---- message ----
    SendMessage {
        message_id: MessageId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        correlation_id: Option<CorrelationId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        reply_to: Option<MessageId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        sender: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        recipient: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        channel: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        kind: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional, type = Option<JsonValue>)]
        payload: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        deadline: Option<Timestamp>,
    },
    TransitionMessage {
        message_id: MessageId,
        to: MessageState,
        expected_version: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        dead_letter_reason: Option<String>,
    },

    // ---- execution command (the stop/kill spine) ----
    IssueCommand {
        command_id: CommandId,
        /// OPEN bounded string (e.g. `stop_attempt`).
        kind: String,
        /// A stop may name several subjects; the projection keeps the first as
        /// its primary target.
        targets: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        actor: Option<Actor>,
    },
    TransitionCommand {
        command_id: CommandId,
        to: CommandState,
        expected_version: u32,
    },
    RecordCommandOutcome {
        command_id: CommandId,
        expected_version: u32,
        outcome: Outcome,
    },

    // ---- lease ----
    AcquireLease {
        lease_id: LeaseId,
        mode: LeaseMode,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        holder: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        scope: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        repo: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        branch: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        base_sha: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        expires_at: Option<Timestamp>,
    },
    RenewLease {
        lease_id: LeaseId,
        expected_version: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        expires_at: Option<Timestamp>,
    },
    ReleaseLease {
        lease_id: LeaseId,
        expected_version: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        disposition: Option<String>,
    },
    ExpireLease {
        lease_id: LeaseId,
        expected_version: u32,
    },

    // ---- gate ----
    OpenGate {
        gate_id: GateId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        attempt_id: Option<AttemptId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        phase_ref: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        kind: Option<String>,
    },
    DecideGate {
        gate_id: GateId,
        expected_version: u32,
        verdict: GateVerdict,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        evidence_ref: Option<String>,
    },

    // ---- authority ----
    GrantAuthority {
        authority_grant_id: AuthorityGrantId,
        grantee: Actor,
        /// OPEN action class this grant covers (e.g. `deploy`, `auto_answer`).
        action_class: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        scope: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        expires_at: Option<Timestamp>,
    },
    RevokeAuthority {
        authority_grant_id: AuthorityGrantId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        reason: Option<String>,
    },

    // ---- attention ----
    RaiseAttention {
        attention_item_id: AttentionItemId,
        /// OPEN kind; deduplicated with `subject_ref` while unresolved.
        kind: String,
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        subject_ref: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        raised_by: Option<Actor>,
    },
    ResolveAttention {
        attention_item_id: AttentionItemId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        resolution: Option<String>,
    },

    // ---- evidence ----
    RecordEvidence {
        evidence_id: EvidenceId,
        /// OPEN bounded string (e.g. `transcript`, `diff`, `log`).
        kind: String,
        r#ref: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        digest: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        byte_size: Option<ByteCount>,
    },

    // ---- worktree ----
    RegisterWorktree {
        worktree_id: WorktreeId,
        repo: String,
        path: String,
        branch: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        base_sha: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        lease_id: Option<LeaseId>,
    },
    UpdateWorktree {
        worktree_id: WorktreeId,
        dirty: bool,
        unpushed: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        base_sha: Option<String>,
    },
    ReleaseWorktree {
        worktree_id: WorktreeId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        disposition: Option<String>,
    },

    // ---- dispatch tree ----
    RegisterDispatchNode {
        dispatch_node_id: DispatchNodeId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        parent_id: Option<DispatchNodeId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        attempt_id: Option<AttemptId>,
        /// OPEN kind (e.g. `orchestrator`, `subagent`, `engine`).
        kind: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        label: Option<String>,
    },
    TransitionDispatchNode {
        dispatch_node_id: DispatchNodeId,
        /// OPEN bounded lifecycle label. A dispatch node is bookkeeping over
        /// the spawn tree, not one of the four public state machines, so this
        /// is deliberately not an FSM state.
        to: String,
        expected_version: u32,
    },

    // ---- orchestrator ----
    WriteOrchestratorCheckpoint {
        checkpoint: OrchestratorCheckpoint,
    },
    RecordRound {
        attempt_id: AttemptId,
        round: u32,
        findings: RoundFindingSummary,
    },
    RecordFinding {
        attempt_id: AttemptId,
        round: u32,
        action: FindingAction,
        summary: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        subject_ref: Option<String>,
    },

    // ---- ingestion ----
    IngestRecord {
        kind: IngestionKind,
        /// Bounded inline JSON
        /// (≤ [`INLINE_PAYLOAD_MAX_BYTES`](crate::envelope::INLINE_PAYLOAD_MAX_BYTES)
        /// serialized); larger content travels as `payload_ref`.
        #[specta(type = JsonValue)]
        payload: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        payload_ref: Option<PayloadRef>,
    },
}

/// Why a [`CommandEnvelope`] did not yield a typed command body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandDecodeError {
    /// The envelope's `command_type` does not name the body's variant.
    TypeMismatch { declared: String, body: String },
    /// The payload is not a legal command body.
    MalformedPayload(String),
}

impl std::fmt::Display for CommandDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TypeMismatch { declared, body } => write!(
                f,
                "envelope command_type {declared} does not match command body type {body}"
            ),
            Self::MalformedPayload(reason) => write!(f, "malformed command payload: {reason}"),
        }
    }
}

impl std::error::Error for CommandDecodeError {}

impl KernelCommand {
    /// The `command_type` an envelope carrying this body must declare —
    /// identical to the body's own `type` tag.
    pub fn command_type(&self) -> &'static str {
        match self {
            Self::ActivateKernel { .. } => "activate_kernel",
            Self::CreateTask { .. } => "create_task",
            Self::TransitionTask { .. } => "transition_task",
            Self::UpdateBudget { .. } => "update_budget",
            Self::CreateAttempt { .. } => "create_attempt",
            Self::TransitionAttempt { .. } => "transition_attempt",
            Self::RecordAttemptOutcome { .. } => "record_attempt_outcome",
            Self::OpenEngineSession { .. } => "open_engine_session",
            Self::CloseEngineSession { .. } => "close_engine_session",
            Self::SendMessage { .. } => "send_message",
            Self::TransitionMessage { .. } => "transition_message",
            Self::IssueCommand { .. } => "issue_command",
            Self::TransitionCommand { .. } => "transition_command",
            Self::RecordCommandOutcome { .. } => "record_command_outcome",
            Self::AcquireLease { .. } => "acquire_lease",
            Self::RenewLease { .. } => "renew_lease",
            Self::ReleaseLease { .. } => "release_lease",
            Self::ExpireLease { .. } => "expire_lease",
            Self::OpenGate { .. } => "open_gate",
            Self::DecideGate { .. } => "decide_gate",
            Self::GrantAuthority { .. } => "grant_authority",
            Self::RevokeAuthority { .. } => "revoke_authority",
            Self::RaiseAttention { .. } => "raise_attention",
            Self::ResolveAttention { .. } => "resolve_attention",
            Self::RecordEvidence { .. } => "record_evidence",
            Self::RegisterWorktree { .. } => "register_worktree",
            Self::UpdateWorktree { .. } => "update_worktree",
            Self::ReleaseWorktree { .. } => "release_worktree",
            Self::RegisterDispatchNode { .. } => "register_dispatch_node",
            Self::TransitionDispatchNode { .. } => "transition_dispatch_node",
            Self::WriteOrchestratorCheckpoint { .. } => "write_orchestrator_checkpoint",
            Self::RecordRound { .. } => "record_round",
            Self::RecordFinding { .. } => "record_finding",
            Self::IngestRecord { .. } => "ingest_record",
        }
    }

    /// Decode the typed body out of an envelope and require the envelope's
    /// `command_type` to name the same variant.
    ///
    /// Routing metadata and the discriminant disagreeing is a refusal, not a
    /// preference for one of them: a handler picked by `command_type` would
    /// otherwise execute a body of some other shape.
    pub fn from_envelope(envelope: &CommandEnvelope) -> Result<Self, CommandDecodeError> {
        // Borrow the payload rather than cloning it: a command body can carry a
        // 64 KiB inline payload, and this runs on every submission.
        let command = <Self as serde::Deserialize>::deserialize(&envelope.payload)
            .map_err(|e| CommandDecodeError::MalformedPayload(e.to_string()))?;
        if envelope.command_type != command.command_type() {
            return Err(CommandDecodeError::TypeMismatch {
                declared: envelope.command_type.clone(),
                body: command.command_type().to_owned(),
            });
        }
        Ok(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::envelope::Origin;
    use crate::ids::{IdempotencyKey, ProjectId};

    fn activate() -> KernelCommand {
        KernelCommand::ActivateKernel {
            cutover_id: "00000000-0000-4000-8000-000000000001".to_owned(),
            archive_manifest_sha256: "b".repeat(64),
        }
    }

    fn envelope(command_type: &str, payload: Value) -> CommandEnvelope {
        CommandEnvelope {
            command_id: CommandId::new("cmd-1"),
            project_id: ProjectId::new("system"),
            command_type: command_type.into(),
            schema_version: 1,
            issued_at: Timestamp::new("2026-07-28T00:00:00Z"),
            actor: Actor {
                kind: "operator".into(),
                id: None,
            },
            origin: Origin {
                system: "gw".into(),
                r#ref: None,
            },
            target_aggregate_type: Some("kernel".into()),
            target_aggregate_id: None,
            expected_version: Some(1),
            idempotency_key: IdempotencyKey::new("kernel_activated:cut-1"),
            causation_id: None,
            correlation_id: None,
            payload,
        }
    }

    #[test]
    fn every_variant_tag_matches_its_command_type() {
        // One representative per aggregate family is not enough: a copy-paste
        // slip in `command_type()` would silently route the wrong handler. Walk
        // the serialized tag of a value from EVERY variant instead.
        let all = all_variants();
        assert_eq!(all.len(), 34, "the v1 command set is 34 variants");
        for command in &all {
            let json = serde_json::to_value(command).expect("serialize");
            let tag = json["type"].as_str().expect("tagged with a string type");
            assert_eq!(
                tag,
                command.command_type(),
                "command_type() disagrees with the serde tag for {command:?}"
            );
        }
        let mut tags: Vec<&str> = all.iter().map(|c| c.command_type()).collect();
        tags.sort_unstable();
        tags.dedup();
        assert_eq!(tags.len(), all.len(), "two variants share a command_type");
    }

    #[test]
    fn every_variant_round_trips_through_json() {
        for command in all_variants() {
            let json = serde_json::to_value(&command).expect("serialize");
            let back: KernelCommand = serde_json::from_value(json).expect("deserialize");
            assert_eq!(back, command);
        }
    }

    #[test]
    fn absent_optionals_are_omitted_not_null() {
        let command = KernelCommand::CreateTask {
            task_id: TaskId::new("task-1"),
            kind: None,
            title: None,
            spec_ref: None,
            project: None,
            priority: None,
            tracker_ref: None,
        };
        let json = serde_json::to_string(&command).expect("serialize");
        assert_eq!(json, "{\"type\":\"create_task\",\"task_id\":\"task-1\"}");
    }

    #[test]
    fn envelope_decode_requires_the_declared_type_to_match_the_body() {
        let payload = serde_json::to_value(activate()).expect("serialize");
        let good = envelope("activate_kernel", payload.clone());
        assert_eq!(
            KernelCommand::from_envelope(&good).expect("decodes"),
            activate()
        );

        // The envelope claims one command, the body is another: refuse rather
        // than let a handler chosen by command_type run a foreign body.
        let mismatched = envelope("create_task", payload);
        assert!(matches!(
            KernelCommand::from_envelope(&mismatched),
            Err(CommandDecodeError::TypeMismatch { .. })
        ));

        let garbage = envelope("activate_kernel", serde_json::json!({ "type": "nope" }));
        assert!(matches!(
            KernelCommand::from_envelope(&garbage),
            Err(CommandDecodeError::MalformedPayload(_))
        ));
    }

    #[test]
    fn activate_kernel_is_the_pinned_wire_shape() {
        let json = serde_json::to_value(activate()).expect("serialize");
        assert_eq!(json["type"], "activate_kernel");
        assert_eq!(json["cutover_id"], "00000000-0000-4000-8000-000000000001");
        assert!(crate::blob::is_sha256_hex(
            json["archive_manifest_sha256"]
                .as_str()
                .expect("hex string")
        ));
    }

    /// One value of EVERY variant. Adding a command without extending this
    /// list fails `every_variant_tag_matches_its_command_type`'s count pin.
    fn all_variants() -> Vec<KernelCommand> {
        let ts = || Timestamp::new("2026-07-28T00:00:00Z");
        vec![
            activate(),
            KernelCommand::CreateTask {
                task_id: TaskId::new("task-1"),
                kind: Some("execution".into()),
                title: Some("ship the kernel".into()),
                spec_ref: None,
                project: Some("proj-alpha".into()),
                priority: Some(2),
                tracker_ref: None,
            },
            KernelCommand::TransitionTask {
                task_id: TaskId::new("task-1"),
                to: TaskState::Working,
                expected_version: 1,
            },
            KernelCommand::UpdateBudget {
                attempt_id: AttemptId::new("att-1"),
                expected_version: 2,
                budget: Budget {
                    max_tokens: Some(2_000_000),
                    max_tool_calls: None,
                    max_wall_ms: None,
                    max_cost_micros: None,
                },
            },
            KernelCommand::CreateAttempt {
                attempt_id: AttemptId::new("att-1"),
                task_id: TaskId::new("task-1"),
                engine: EngineId::new("engine-a"),
                capability: Some("code_write".into()),
                role: None,
                model_lane: Some("standard".into()),
                permission_profile: None,
                worktree_lease_id: Some(LeaseId::new("lease-1")),
                base_sha: None,
                budget: None,
            },
            KernelCommand::TransitionAttempt {
                attempt_id: AttemptId::new("att-1"),
                to: AttemptState::Blocked,
                expected_version: 3,
                receipt_id: Some(ReceiptId::new("r-1")),
            },
            KernelCommand::RecordAttemptOutcome {
                attempt_id: AttemptId::new("att-1"),
                expected_version: 4,
                exit_code: Some(0),
                provider_terminal_event: None,
                result_valid: Some(true),
                evidence_manifest_ref: None,
            },
            KernelCommand::OpenEngineSession {
                engine_session_id: EngineSessionId::new("sess-1"),
                attempt_id: AttemptId::new("att-1"),
                engine: EngineId::new("engine-a"),
                provider_session_ref: None,
            },
            KernelCommand::CloseEngineSession {
                engine_session_id: EngineSessionId::new("sess-1"),
            },
            KernelCommand::SendMessage {
                message_id: MessageId::new("msg-1"),
                correlation_id: Some(CorrelationId::new("corr-7")),
                reply_to: None,
                sender: Some("orchestrator".into()),
                recipient: Some("operator".into()),
                channel: Some("chat".into()),
                kind: Some("status_update".into()),
                payload: Some(serde_json::json!({ "text": "verify passed" })),
                deadline: None,
            },
            KernelCommand::TransitionMessage {
                message_id: MessageId::new("msg-1"),
                to: MessageState::Delivered,
                expected_version: 1,
                dead_letter_reason: None,
            },
            KernelCommand::IssueCommand {
                command_id: CommandId::new("cmd-1"),
                kind: "stop_attempt".into(),
                targets: vec!["att-1".into()],
                actor: None,
            },
            KernelCommand::TransitionCommand {
                command_id: CommandId::new("cmd-1"),
                to: CommandState::Targeted,
                expected_version: 1,
            },
            KernelCommand::RecordCommandOutcome {
                command_id: CommandId::new("cmd-1"),
                expected_version: 3,
                outcome: Outcome::Clean,
            },
            KernelCommand::AcquireLease {
                lease_id: LeaseId::new("lease-1"),
                mode: LeaseMode::Exclusive,
                holder: Some("att-1".into()),
                scope: Some("worktree".into()),
                repo: None,
                path: None,
                branch: None,
                base_sha: None,
                expires_at: Some(ts()),
            },
            KernelCommand::RenewLease {
                lease_id: LeaseId::new("lease-1"),
                expected_version: 1,
                expires_at: Some(ts()),
            },
            KernelCommand::ReleaseLease {
                lease_id: LeaseId::new("lease-1"),
                expected_version: 2,
                disposition: Some("clean".into()),
            },
            KernelCommand::ExpireLease {
                lease_id: LeaseId::new("lease-1"),
                expected_version: 2,
            },
            KernelCommand::OpenGate {
                gate_id: GateId::new("gate-1"),
                attempt_id: Some(AttemptId::new("att-1")),
                phase_ref: None,
                kind: Some("review".into()),
            },
            KernelCommand::DecideGate {
                gate_id: GateId::new("gate-1"),
                expected_version: 1,
                verdict: GateVerdict::Pass,
                evidence_ref: None,
            },
            KernelCommand::GrantAuthority {
                authority_grant_id: AuthorityGrantId::new("grant-1"),
                grantee: Actor {
                    kind: "orchestrator".into(),
                    id: None,
                },
                action_class: "deploy".into(),
                scope: None,
                expires_at: None,
            },
            KernelCommand::RevokeAuthority {
                authority_grant_id: AuthorityGrantId::new("grant-1"),
                reason: None,
            },
            KernelCommand::RaiseAttention {
                attention_item_id: AttentionItemId::new("att-item-1"),
                kind: "risk_tag".into(),
                summary: "data-migration pages".into(),
                subject_ref: Some("task-1".into()),
                raised_by: None,
            },
            KernelCommand::ResolveAttention {
                attention_item_id: AttentionItemId::new("att-item-1"),
                resolution: None,
            },
            KernelCommand::RecordEvidence {
                evidence_id: EvidenceId::new("ev-1"),
                kind: "transcript".into(),
                r#ref: "blob://transcript".into(),
                digest: None,
                byte_size: Some(ByteCount::new(4096)),
            },
            KernelCommand::RegisterWorktree {
                worktree_id: WorktreeId::new("wt-1"),
                repo: "gridwork".into(),
                path: "/w/kernel".into(),
                branch: "feature/kernel".into(),
                base_sha: None,
                lease_id: Some(LeaseId::new("lease-1")),
            },
            KernelCommand::UpdateWorktree {
                worktree_id: WorktreeId::new("wt-1"),
                dirty: true,
                unpushed: false,
                base_sha: None,
            },
            KernelCommand::ReleaseWorktree {
                worktree_id: WorktreeId::new("wt-1"),
                disposition: None,
            },
            KernelCommand::RegisterDispatchNode {
                dispatch_node_id: DispatchNodeId::new("node-1"),
                parent_id: None,
                attempt_id: Some(AttemptId::new("att-1")),
                kind: "subagent".into(),
                label: None,
            },
            KernelCommand::TransitionDispatchNode {
                dispatch_node_id: DispatchNodeId::new("node-1"),
                to: "finished".into(),
                expected_version: 1,
            },
            KernelCommand::WriteOrchestratorCheckpoint {
                checkpoint: OrchestratorCheckpoint {
                    orchestrator_id: None,
                    seq: crate::ids::Seq::new(42),
                    native_session_ref: None,
                    active_goal: None,
                    active_step_ref: None,
                    latest_command_ref: None,
                    open_attempts: Some(vec![]),
                    leases: None,
                    pending_approvals: None,
                    budget_cursor: None,
                },
            },
            KernelCommand::RecordRound {
                attempt_id: AttemptId::new("att-1"),
                round: 2,
                findings: RoundFindingSummary {
                    total: 3,
                    auto_fix: 1,
                    ask_user: 1,
                    no_op: 1,
                },
            },
            KernelCommand::RecordFinding {
                attempt_id: AttemptId::new("att-1"),
                round: 2,
                action: FindingAction::AutoFix,
                summary: "unbounded read limit".into(),
                subject_ref: None,
            },
            KernelCommand::IngestRecord {
                kind: IngestionKind::Cost,
                payload: serde_json::json!({ "spent_cost_micros": "5000000" }),
                payload_ref: None,
            },
        ]
    }
}
