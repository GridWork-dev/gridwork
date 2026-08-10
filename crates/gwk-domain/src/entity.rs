//! Entity snapshots — the vertical-slice record shapes.
//!
//! These are the wire/storage projections of each aggregate at a version, not
//! the event stream itself. Conventions (see `docs/contract/NAMING.md`):
//! snake_case fields, absent optionals omitted, refs opaque strings, open
//! classification fields are bounded strings (never closed enums), and every
//! CAS-transitioned entity carries `version`.

use std::collections::BTreeMap;

use crate::envelope::{Actor, PayloadRef};
use crate::fsm::{
    AttemptState, CommandState, GateVerdict, LeaseMode, LeaseState, MessageState, Outcome,
    TaskState,
};
use crate::ids::{
    AttemptId, AttentionItemId, AuthorityGrantId, ByteCount, CommandId, CorrelationId, CostEntryId,
    CostMicros, DispatchNodeId, EngineId, EngineSessionId, EvidenceId, FenceToken, GateId,
    IdempotencyKey, IngestedRecordId, LeaseId, MessageId, PtySessionGeneration, PtySessionId,
    ReceiptId, Seq, TaskId, Timestamp, TokenCount, WorkflowRunId, WorkspaceNodeId, WorktreeId,
};
use crate::ingestion::IngestionKind;

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
    /// A relayed permission prompt is a gate: the engine's question and its
    /// offered options arrive on open, the chosen option on decide. All three
    /// absent for gates that are not prompts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub question: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub options: Option<Vec<String>>,
    pub verdict: GateVerdict,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub chosen_option: Option<String>,
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
    // Set together when the grant is withdrawn. Absent means live: a grant with
    // no revocation stamp is one that still matches, subject only to expiry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub revoked_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub revoke_reason: Option<String>,
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

/// One engine cost report — an append-only fact in the spend ledger, never an
/// accumulator. Totals are a rollup query, so a token report never rides the
/// CAS version an FSM transition uses.
///
/// Token counts are the universal fact; currency is optional and
/// engine-reported (`cost_is_estimate` qualifies it), never derived by
/// conversion. At least one of the three subject refs is present.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct CostEntry {
    pub id: CostEntryId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub attempt_id: Option<AttemptId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub engine_session_id: Option<EngineSessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub dispatch_node_id: Option<DispatchNodeId>,
    pub engine: EngineId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub input_tokens: Option<TokenCount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub cached_input_tokens: Option<TokenCount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub cache_write_tokens: Option<TokenCount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub output_tokens: Option<TokenCount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub reasoning_tokens: Option<TokenCount>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub cost_micros: Option<CostMicros>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub cost_is_estimate: Option<bool>,
    pub recorded_at: Timestamp,
}

/// One record that entered the log through ingestion rather than through an
/// aggregate's lifecycle.
///
/// Immutable like [`Evidence`] — no `version`, no `state`, written once. Its
/// `kind` is the CLOSED [`IngestionKind`] set, unlike the open classification
/// strings elsewhere in this contract: the absence of an import path is
/// load-bearing here, and an open string would be that door.
///
/// `payload` and `payload_ref` are not exclusive. The inline half is bounded
/// (the append path enforces
/// [`INLINE_PAYLOAD_MAX_BYTES`](crate::envelope::INLINE_PAYLOAD_MAX_BYTES)) and
/// carries either the whole content or a descriptor of the blob beside it —
/// a graph snapshot's node and edge counts are worth having in the projection
/// without fetching ninety megabytes to learn them.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct IngestedRecord {
    pub id: IngestedRecordId,
    pub kind: IngestionKind,
    #[specta(type = crate::envelope::JsonValue)]
    pub payload: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub payload_ref: Option<PayloadRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub ingested_by: Option<Actor>,
    /// Where this record's event sits in the log — the pointer back to the one
    /// thing that can prove the row, since ingestion writes no transition.
    pub event_seq: Seq,
    pub ingested_at: Timestamp,
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
    /// Presentation rank, mirroring [`Task::priority`] exactly — same type,
    /// same optionality, same "absent means unranked".
    ///
    /// This is RANK ONLY. It is deliberately NOT cost-of-waiting: a queue
    /// ordered by urgency-times-age needs `raised_at` weighted against a
    /// per-kind decay the contract does not model, and that clause is
    /// EXPLICITLY DEFERRED rather than covered here. Reading this column as
    /// "ranking is solved" is the mistake it is worth naming: a client sorting
    /// on it alone still shows a week-old low-priority page below a minute-old
    /// high-priority one forever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub priority: Option<i32>,
    pub raised_at: Timestamp,
    /// Seen, not handled. An ack quiets the row's presentation and NOTHING
    /// else: `resolved_at` stays absent, so the item still holds its
    /// (kind, subject_ref) dedup slot and the same problem still cannot raise
    /// a second time. Mute is the same stamp with a deadline on it.
    ///
    /// Both are idempotent last-write-wins: acking twice moves the stamp and
    /// is not an error, which is why neither carries an expected version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub acked_at: Option<Timestamp>,
    /// Quiet until this instant. Past-or-absent means audible; a client
    /// unmutes by stamping a time already gone rather than by a second verb.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub muted_until: Option<Timestamp>,
    // Set together when someone closes the item. While `resolved_at` is absent
    // the item is what the (kind, subject_ref) dedup counts, so it is also the
    // field that decides whether the same problem raises a second time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub resolved_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub resolution: Option<String>,
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
    // Set together when the tree is handed back. Absent means still held: a
    // worktree with no release stamp is one somebody may still be working in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub released_at: Option<Timestamp>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub disposition: Option<String>,
    pub created_at: Timestamp,
}

/// The closed structural levels in a durable workspace tree.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    specta::Type,
)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceNodeKind {
    Workspace,
    Tab,
    Pane,
}

impl WorkspaceNodeKind {
    /// The wire spelling — the same string `serde` writes, held as one method
    /// so the projector's column value and refusal prose can never drift from
    /// the tag on the wire.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::Tab => "tab",
            Self::Pane => "pane",
        }
    }
}

/// One durable node in the workspace arrangement.
///
/// Only structure belongs here: the host owns split sizes, focus, and z-order.
/// `session_id` is present only for panes; parentage and binding rules are
/// enforced by the kernel and projection schema.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceNode {
    pub id: WorkspaceNodeId,
    pub version: u32,
    pub kind: WorkspaceNodeKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub parent_id: Option<WorkspaceNodeId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub session_id: Option<crate::ids::PtySessionId>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// One run of a workflow: the choreography as a kernel ledger object.
///
/// The RUN, not the template (S3): `template_ref` is an opaque reference to
/// client-side data, and `step` is an open string because the act taxonomy is
/// template data (decision 17) — the kernel asserts nothing about what the
/// steps are called. Closed runs keep their row; the Board reads history from
/// it and a replay rebuilds it from the log.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct WorkflowRun {
    pub id: WorkflowRunId,
    pub version: u32,
    /// `running` until closed; a close records the caller's outcome here.
    pub state: String,
    /// The current step of the choreography — template data, never a kernel
    /// vocabulary. Absent until the first advance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub step: Option<String>,
    /// Which client-side template this run follows. Opaque to the kernel.
    pub template_ref: String,
    /// The template content digest at open time, when the caller pinned one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub template_sha256: Option<String>,
    /// The task this run advances, when it advances one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub task_id: Option<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub title: Option<String>,
    pub opened_at: Timestamp,
    pub updated_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub closed_at: Option<Timestamp>,
}

/// One lifetime of a hosted PTY session as a kernel ledger object (P17).
///
/// Lifecycle authority, not telemetry (S6 stands): the pty-host records
/// publish/retire and attach/detach here so the S8 cutover receipt can be
/// read from the log — peak concurrency against B4's floor from open/close
/// intervals, attach/detach counts from the counters, host restarts from
/// distinct generation tokens. Closed sessions keep their row; a replay
/// rebuilds it from the log.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct PtySession {
    pub id: PtySessionId,
    pub version: u32,
    /// `running` until closed; a close writes `closed`.
    pub state: String,
    /// The host lifetime this session belongs to. A reclaimed id under a new
    /// generation is a NEW aggregate lifetime on the wire, but ledger ids are
    /// unique per open — the generation is recorded, never re-keyed.
    pub generation: PtySessionGeneration,
    /// Live attaches opened against this session over its lifetime.
    pub attach_count: u32,
    /// Attach ends recorded against this session over its lifetime.
    pub detach_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub title: Option<String>,
    pub opened_at: Timestamp,
    pub updated_at: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub closed_at: Option<Timestamp>,
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
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub parent_id: Option<DispatchNodeId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub attempt_id: Option<AttemptId>,
    pub kind: String,
    // OPEN bounded lifecycle label, NOT an FSM state: the spawn tree has no
    // seeded edge set, so storage guards the version CAS and nothing else.
    // Plain `//` — a doc attribute here would flow into the bindings.ts golden.
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub label: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// The state a dispatch node is born in — the spawn tree's only fixed label.
pub const DISPATCH_NODE_INITIAL_STATE: &str = "registered";

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
