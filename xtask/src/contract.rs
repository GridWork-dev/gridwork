//! `cargo run -p xtask -- contract [--check]` — the contract codegen gate.
//!
//! Generates, deterministically (no timestamps, no local paths):
//!   * `contracts/bindings.ts` — the TypeScript contract from gwk-context + gwk-domain + gwk-theme
//!   * `contracts/signal-theme.json` — the SIGNAL tokens as data
//!   * `contracts/goldens/*.json` — Rust-serialized fixtures the bun tests decode
//!
//! `--check` regenerates in memory and fails on ANY drift from the committed
//! artifacts, then round-trips `contracts/goldens-ts/*.json` (re-emitted by the
//! bun tests, committed) back through serde to prove TS -> Rust decoding agrees.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use gwk_context::{
    Assurance, CandidateDisposition, CompareSubject, ContextAggregate, ContextAttribution,
    ContextEventName, ContextEventPayload, ContextFact, ContextQuery, ContextStage,
    Digest as ContextDigest, EvidenceRefs, ExplainSubject, FinalizationSupplement,
    FinalizationSupplementId, ManifestId, ManifestSelector, ObservationIndex,
    ObservationSupplement, ObservationSupplementId, Participation, ParticipationReason,
    ParticipationRecords, RecordContextFact, RecordCount, ReleaseSupplement, ReleaseSupplementId,
    ResolvedManifest, SupplementKind, VerificationVerdict,
};
use gwk_domain::blob::{BlobAddress, BlobDescriptor};
use gwk_domain::checkpoint::{CHECKPOINT_SCHEMA_VERSION, Checkpoint};
use gwk_domain::command::KernelCommand;
use gwk_domain::entity::{
    Attempt, Budget, Command, Message, PtySession, PtySessionTemplate, Task, WorkflowRun,
    WorkspaceNode, WorkspaceNodeKind,
};
use gwk_domain::envelope::{Actor, CommandEnvelope, EventEnvelope, Origin, PayloadRef};
use gwk_domain::frame::{
    CellColor, CellStyle, CellUnderline, PtyAnsiSlot, PtyCellUpdate, PtyCursor, PtyDelta, PtyFrame,
    PtyRun,
};
use gwk_domain::fsm::{AttemptState, CommandState, MessageState, Outcome, TaskState};
use gwk_domain::ids::{
    AggregateId, AttemptId, BlobUploadId, ByteCount, CommandId, CorrelationId, CostMicros,
    EngineId, EngineSessionId, EventCount, EventId, EvidenceId, IdempotencyKey, LeaseId, MessageId,
    ProjectId, PtyFrameSeq, PtySessionGeneration, PtySessionId, PtySessionTemplateName, RequestId,
    Seq, TaskId, Timestamp, WorkflowRunId, WorkspaceNodeId,
};
use gwk_domain::inherited::{BudgetCursor, OrchestratorCheckpoint, PendingApproval};
use gwk_domain::protocol::{
    CapabilityName, ClientControl, FrameKind, KernelErrorCode, KernelRequest, KernelResult,
    PROTOCOL_MINOR, ProjectionKind, ProtocolVersion, PtyInputData, ServerControl,
};
use gwk_domain::transition::TransitionResult;

const HEADER: &str = "\
// GridWork contract bindings.
// Generated from gwk-context + gwk-domain + gwk-theme by `cargo run -p xtask -- contract`.
// DO NOT EDIT — regenerate instead; CI diffs this file against the source.
";

fn repo_root() -> PathBuf {
    // xtask always runs from the workspace via cargo; CARGO_MANIFEST_DIR is xtask/.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask has a parent dir")
        .to_path_buf()
}

const CONTEXT_TRUTH_ROOTS: [&str; 4] = [
    "ResolvedManifest",
    "ReleaseSupplement",
    "ObservationSupplement",
    "FinalizationSupplement",
];

const CONTEXT_TRUTH_TABLES: [&str; 4] = [
    "gwk.context_manifest",
    "gwk.context_release",
    "gwk.context_observation",
    "gwk.context_finalization",
];

const CONTEXT_CARDINALITY_CONSTRAINTS: [&str; 4] = [
    "context_manifest_one_per_attempt",
    "context_release_one_per_manifest",
    "context_observation_stable_order",
    "context_finalization_one_per_manifest",
];

const CONTEXT_VALUE_VALIDATORS: [&str; 2] = [
    "gwk.context_evidence_ids_are_valid",
    "gwk.context_participations_are_valid",
];

const CONTEXT_EVIDENCE_GUARD: &str = "CHECK (gwk.context_evidence_ids_are_valid(evidence_ids))";
const CONTEXT_PARTICIPATION_GUARD: &str =
    "CHECK (gwk.context_participations_are_valid(participations))";
const CONTEXT_VALUE_GUARD_COUNT: usize = 5;

const CONTEXT_MUTATION_TRIGGERS: [&str; 8] = [
    "context_manifest_append_only",
    "context_manifest_no_truncate",
    "context_release_append_only",
    "context_release_no_truncate",
    "context_observation_append_only",
    "context_observation_no_truncate",
    "context_finalization_append_only",
    "context_finalization_no_truncate",
];

/// Field names that would let a caller assert who it is.
///
/// Compared against whole TypeScript property names, never as substrings. A
/// substring sweep for `author` matches inside `authority_digest`, and the
/// tempting repair is to drop the word — which deletes the check for one of the
/// names a spoofed record is most likely to use.
const CONTEXT_ATTRIBUTION_FIELDS: [&str; 8] = [
    "actor",
    "author",
    "principal",
    "identity",
    "user",
    "requested_by",
    "submitted_by",
    "on_behalf_of",
];

/// The generated types a client can construct and submit.
const CONTEXT_CLIENT_SUBMITTABLE: [&str; 4] = [
    "RecordContextFact_Serialize",
    "RecordContextFact_Deserialize",
    "ContextFact_Serialize",
    "ContextFact_Deserialize",
];

/// CTX-12, enforced against the GENERATED surface rather than against a list.
///
/// The Rust-side test that preceded this swept a hand-written `vec![]` holding
/// one fact per variant. An eleventh variant carrying an `actor: String`, mapped
/// onto an existing event name, passed every test in the crate: the vec still
/// held ten entries so the count assertion still held, and the sweep never saw
/// the new shape. The guard covered the variants somebody remembered.
///
/// The generated TypeScript enumerates every variant and every field by
/// construction, so reading it covers the eleventh variant the day it is added
/// without anyone maintaining a second list. Returns the number of declarations
/// inspected: zero means this stopped finding its subject, which is a broken
/// guard rather than a clean grammar.
fn inspect_context_attribution(exported: &str) -> Result<usize, String> {
    assert!(
        !CONTEXT_CLIENT_SUBMITTABLE.is_empty() && !CONTEXT_ATTRIBUTION_FIELDS.is_empty(),
        "the CTX-12 expectation sets must not be empty"
    );
    let mut inspected = 0usize;
    for name in CONTEXT_CLIENT_SUBMITTABLE {
        let needle = format!("export type {name} =");
        let Some(start) = exported.find(&needle) else {
            return Err(format!(
                "inspected {inspected} client-submittable context types; {name} is missing"
            ));
        };
        let rest = &exported[start + needle.len()..];
        let end = rest.find("\nexport ").unwrap_or(rest.len());
        let decl = &rest[..end];
        for field in CONTEXT_ATTRIBUTION_FIELDS {
            // Whole property names only: a token that IS `field:` or `field?:`,
            // never a substring of a longer identifier.
            let declared = decl.split_whitespace().any(|token| {
                let token = token.trim_start_matches(['{', '(', '|']);
                token == format!("{field}:") || token == format!("{field}?:")
            });
            if declared {
                return Err(format!(
                    "{name} declares `{field}`: a client could assert its own provenance (CTX-12)"
                ));
            }
        }
        inspected += 1;
    }
    if inspected != CONTEXT_CLIENT_SUBMITTABLE.len() {
        return Err(format!(
            "inspected {inspected} client-submittable context types; expected {}",
            CONTEXT_CLIENT_SUBMITTABLE.len()
        ));
    }
    Ok(inspected)
}

fn bindings_body(source: &str) -> Result<&str, String> {
    // Include real newlines so this search cannot match this string literal.
    let signature = "\nfn bindings() -> String {\n";
    let start = source
        .find(signature)
        .ok_or_else(|| "bindings() signature is missing".to_owned())?;
    let close = source[start..]
        .find("\n}\n")
        .ok_or_else(|| "bindings() closing brace is missing".to_owned())?;
    Ok(&source[start..start + close])
}

fn inspect_context_truth_registry(source: &str) -> Result<usize, String> {
    assert!(
        !CONTEXT_TRUTH_ROOTS.is_empty(),
        "context truth-root expectation set must not be empty"
    );
    let body = bindings_body(source)?;
    let present = CONTEXT_TRUTH_ROOTS
        .iter()
        .filter(|name| body.contains(&format!(".register::<{name}>()")))
        .count();
    if present != CONTEXT_TRUTH_ROOTS.len() {
        return Err(format!(
            "inspected {present} registered context truth roots; expected {}",
            CONTEXT_TRUTH_ROOTS.len()
        ));
    }
    Ok(present)
}

fn inspect_context_truth_roots(exported: &str) -> Result<usize, String> {
    assert!(
        !CONTEXT_TRUTH_ROOTS.is_empty(),
        "context truth-root expectation set must not be empty"
    );
    let present = CONTEXT_TRUTH_ROOTS
        .iter()
        .filter(|name| exported.contains(&format!("export type {name} =")))
        .count();
    if present != CONTEXT_TRUTH_ROOTS.len() {
        let missing = CONTEXT_TRUTH_ROOTS
            .iter()
            .filter(|name| !exported.contains(&format!("export type {name} =")))
            .copied()
            .collect::<Vec<_>>();
        return Err(format!(
            "inspected {present} context truth roots; expected {}; missing {}",
            CONTEXT_TRUTH_ROOTS.len(),
            missing.join(", ")
        ));
    }
    Ok(present)
}

fn inspect_context_truth_schema(sql: &str) -> Result<(), String> {
    assert!(
        !CONTEXT_TRUTH_TABLES.is_empty()
            && !CONTEXT_CARDINALITY_CONSTRAINTS.is_empty()
            && !CONTEXT_VALUE_VALIDATORS.is_empty()
            && !CONTEXT_MUTATION_TRIGGERS.is_empty(),
        "context schema expectation sets must not be empty"
    );

    let table_count = CONTEXT_TRUTH_TABLES
        .iter()
        .filter(|table| sql.contains(&format!("CREATE TABLE {table} (")))
        .count();
    let constraint_count = CONTEXT_CARDINALITY_CONSTRAINTS
        .iter()
        .filter(|constraint| sql.contains(&format!("CONSTRAINT {constraint}")))
        .count();
    let validator_count = CONTEXT_VALUE_VALIDATORS
        .iter()
        .filter(|validator| {
            sql.matches(&format!("CREATE FUNCTION {validator}("))
                .count()
                == 1
        })
        .count();
    let value_guard_count = sql.matches(CONTEXT_EVIDENCE_GUARD).count()
        + sql.matches(CONTEXT_PARTICIPATION_GUARD).count();
    let trigger_count = CONTEXT_MUTATION_TRIGGERS
        .iter()
        .filter(|trigger| {
            sql.contains(&format!("CREATE TRIGGER {trigger}"))
                && sql.contains(&format!("ENABLE ALWAYS TRIGGER {trigger}"))
        })
        .count();

    if table_count != CONTEXT_TRUTH_TABLES.len()
        || constraint_count != CONTEXT_CARDINALITY_CONSTRAINTS.len()
        || validator_count != CONTEXT_VALUE_VALIDATORS.len()
        || value_guard_count != CONTEXT_VALUE_GUARD_COUNT
        || trigger_count != CONTEXT_MUTATION_TRIGGERS.len()
    {
        return Err(format!(
            "inspected {table_count} context table declarations, {constraint_count} cardinality constraints, {validator_count} value-validator functions, {value_guard_count} value-validation constraints, and {trigger_count} enabled mutation guards; expected {}/{}/{}/{}/{}",
            CONTEXT_TRUTH_TABLES.len(),
            CONTEXT_CARDINALITY_CONSTRAINTS.len(),
            CONTEXT_VALUE_VALIDATORS.len(),
            CONTEXT_VALUE_GUARD_COUNT,
            CONTEXT_MUTATION_TRIGGERS.len()
        ));
    }
    Ok(())
}

/// The generated TypeScript contract.
///
/// This is a MANUAL registry: every `specta::Type`-deriving public type meant
/// to reach `contracts/bindings.ts` must appear in the `.register()` chain
/// below — directly, or as a field reachable from a type that is already
/// there. specta walks reachable fields automatically; it does NOT discover
/// new roots on its own, and nothing else in this codebase notices when an
/// editor adds a wire type to gwk-domain or gwk-theme and forgets to list it
/// here. Adding a new exported type: add its `.register::<T>()` call AND
/// bump `tests::REGISTERED_ROOT_COUNT` below in the same change — that test
/// only pins the registry against silent drift, it does not prove
/// completeness.
fn bindings() -> String {
    inspect_context_truth_registry(include_str!("contract.rs"))
        .unwrap_or_else(|message| panic!("{message}"));
    let types = specta::Types::default()
        .register::<EventEnvelope>()
        .register::<CommandEnvelope>()
        .register::<gwk_domain::entity::Task>()
        .register::<gwk_domain::entity::Attempt>()
        .register::<gwk_domain::entity::EngineSession>()
        .register::<gwk_domain::entity::Message>()
        .register::<gwk_domain::entity::Command>()
        .register::<gwk_domain::entity::Gate>()
        .register::<gwk_domain::entity::AuthorityGrant>()
        .register::<gwk_domain::entity::Receipt>()
        .register::<gwk_domain::entity::Evidence>()
        .register::<gwk_domain::entity::AttentionItem>()
        .register::<gwk_domain::entity::Budget>()
        .register::<gwk_domain::entity::Worktree>()
        .register::<gwk_domain::entity::WorkspaceNode>()
        .register::<gwk_domain::entity::WorkflowRun>()
        .register::<gwk_domain::entity::PtySession>()
        .register::<gwk_domain::entity::PtySessionTemplate>()
        .register::<gwk_domain::entity::Lease>()
        .register::<gwk_domain::entity::DispatchNode>()
        .register::<TransitionResult<TaskState>>()
        .register::<OrchestratorCheckpoint>()
        .register::<gwk_domain::inherited::RoundFindingSummary>()
        // The kernel protocol. The two control unions reach every request,
        // result, projection record, and error code by field walk.
        // `KernelCommand` is NOT among them: it travels as the envelope's
        // `payload`, which the contract types as an opaque JSON tree, so
        // without its own root a TS client would have no type for the thing it
        // has to construct.
        .register::<ClientControl>()
        .register::<ServerControl>()
        .register::<KernelCommand>()
        .register::<Checkpoint>()
        .register::<ResolvedManifest>()
        .register::<ReleaseSupplement>()
        .register::<ObservationSupplement>()
        .register::<FinalizationSupplement>()
        // The wire-v2 Context grammar. Published shapes for a protocol major
        // the kernel refuses — a TS consumer needs the types to be written
        // against before anything speaks them, which is the point of freezing
        // the grammar a phase ahead of its first caller.
        //
        // A type that reaches bindings.ts only as a side effect of something
        // else's reachability is one refactor away from vanishing without a
        // registry diff. That argument was made here and then applied to two
        // types out of ten — every one of these was already being emitted by
        // reachability, and a TS consumer branches on each. `ContextStage` was
        // the sharp case: new to bindings.ts, carried in solely by
        // `Compare.stages`, and restructuring that one field would have removed
        // it from the public surface with nothing in the registry to show it.
        .register::<RecordContextFact>()
        .register::<ContextEventPayload>()
        .register::<ContextFact>()
        .register::<ContextEventName>()
        .register::<ContextAggregate>()
        .register::<ContextQuery>()
        .register::<ContextAttribution>()
        .register::<ContextStage>()
        .register::<SupplementKind>()
        .register::<ManifestSelector>()
        .register::<ExplainSubject>()
        .register::<CompareSubject>()
        .register::<VerificationVerdict>()
        .register::<CandidateDisposition>()
        .register::<gwk_theme::Token>();
    // PhasesFormat, not the unified Format: `skip_serializing_if` (the
    // tri-state omission) is direction-dependent, which unified mode refuses
    // to represent.
    let exported = specta_typescript::Typescript::default()
        .header(HEADER)
        .export(&types, specta_serde::PhasesFormat)
        .expect("typescript export");
    // The exporter leaves a separator space at the end of multiline unions.
    // Generated output is still source: keep it clean under `git diff --check`.
    let normalized = exported
        .split('\n')
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n");
    inspect_context_truth_roots(&normalized).unwrap_or_else(|message| panic!("{message}"));
    inspect_context_attribution(&normalized).unwrap_or_else(|message| panic!("{message}"));
    normalized
}

fn signal_theme_json() -> String {
    let mut out = serde_json::to_string_pretty(gwk_theme::SIGNAL).expect("serialize SIGNAL");
    out.push('\n');
    out
}

// ============================================================
// Golden fixtures — deterministic, hand-pinned values
// ============================================================

fn actor(kind: &str, id: Option<&str>) -> Actor {
    Actor {
        kind: kind.into(),
        id: id.map(Into::into),
    }
}

fn context_digest(nibble: char) -> ContextDigest {
    ContextDigest::from_hex(&nibble.to_string().repeat(64)).expect("golden context digest is valid")
}

fn context_evidence() -> EvidenceRefs {
    EvidenceRefs::new(vec![
        EvidenceId::new("evidence-context-1"),
        EvidenceId::new("evidence-context-2"),
    ])
    .expect("golden context evidence is bounded")
}

fn golden_resolved_manifest() -> ResolvedManifest {
    ResolvedManifest {
        id: ManifestId::parse("manifest-0001").expect("golden manifest id is valid"),
        attempt_id: AttemptId::new("att-context-0001"),
        manifest_digest: context_digest('1'),
        route_digest: context_digest('2'),
        authority_digest: context_digest('3'),
        source_count: RecordCount::new(2).expect("golden source count is bounded"),
        source_bytes: ByteCount::new(768),
        participations: ParticipationRecords::new(vec![
            Participation::active(context_digest('4')),
            Participation::excluded(context_digest('5'), ParticipationReason::BudgetCut)
                .with_detail("excluded after deterministic budget ordering"),
        ])
        .expect("golden participations are valid"),
        evidence_ids: context_evidence(),
        resolved_at: Timestamp::new("2026-08-14T12:00:00Z"),
    }
}

fn golden_release_supplement() -> ReleaseSupplement {
    ReleaseSupplement {
        id: ReleaseSupplementId::parse("release-0001").expect("golden release id is valid"),
        manifest_id: ManifestId::parse("manifest-0001").expect("golden manifest id is valid"),
        rendered_digest: context_digest('6'),
        tool_schema_digest: context_digest('7'),
        rendered_bytes: ByteCount::new(512),
        tool_schema_count: RecordCount::new(3).expect("golden tool count is bounded"),
        evidence_ids: context_evidence(),
        released_at: Timestamp::new("2026-08-14T12:00:01Z"),
    }
}

fn golden_observation_supplement() -> ObservationSupplement {
    ObservationSupplement {
        id: ObservationSupplementId::parse("observation-0001")
            .expect("golden observation id is valid"),
        manifest_id: ManifestId::parse("manifest-0001").expect("golden manifest id is valid"),
        observation_index: ObservationIndex::new(1).expect("golden observation index is valid"),
        fact_digest: context_digest('8'),
        observed_bytes: ByteCount::new(96),
        visible_source_count: RecordCount::new(1).expect("golden visible-source count is bounded"),
        truncated: false,
        evidence_ids: context_evidence(),
        observed_at: Timestamp::new("2026-08-14T12:00:02Z"),
    }
}

fn golden_finalization_supplement() -> FinalizationSupplement {
    FinalizationSupplement {
        id: FinalizationSupplementId::parse("finalization-0001")
            .expect("golden finalization id is valid"),
        manifest_id: ManifestId::parse("manifest-0001").expect("golden manifest id is valid"),
        output_digest: context_digest('9'),
        verification_digest: context_digest('a'),
        approval_count: RecordCount::new(1).expect("golden approval count is bounded"),
        observation_count: RecordCount::new(1).expect("golden observation count is bounded"),
        final_event_root: context_digest('b'),
        lifecycle_complete: true,
        assurance: Assurance::Trace,
        evidence_ids: context_evidence(),
        finalized_at: Timestamp::new("2026-08-14T12:00:03Z"),
    }
}

fn golden_event_envelope_full() -> EventEnvelope {
    EventEnvelope {
        event_id: EventId::new("evt-0001"),
        project_id: ProjectId::new("proj-alpha"),
        aggregate_type: "attempt".into(),
        aggregate_id: AggregateId::new("att-0001"),
        aggregate_version: 4,
        event_type: "attempt_state_changed".into(),
        schema_version: 1,
        global_sequence: Seq::new(9_007_199_254_740_993), // > 2^53: the reason for strings
        occurred_at: Timestamp::new("2026-07-27T12:00:00Z"),
        appended_at: Timestamp::new("2026-07-27T12:00:01Z"),
        actor: actor("liveness_producer", Some("lp-1")),
        origin: Origin {
            system: "kernel".into(),
            r#ref: Some("node-a".into()),
        },
        causation_id: Some(EventId::new("evt-0000")),
        correlation_id: Some(CorrelationId::new("corr-7")),
        idempotency_key: Some(IdempotencyKey::new("flip-once")),
        payload: serde_json::json!({ "from": "running", "to": "blocked", "receipt_id": "r-1" }),
        payload_ref: Some(PayloadRef {
            digest: "sha256:0000000000000000000000000000000000000000000000000000000000000000"
                .into(),
            media_type: "application/octet-stream".into(),
            byte_size: ByteCount::new(1_048_576),
            retention_class: Some("standard".into()),
            evidence_pin: Some(true),
        }),
    }
}

fn golden_event_envelope_minimal() -> EventEnvelope {
    EventEnvelope {
        event_id: EventId::new("evt-0002"),
        project_id: ProjectId::new("proj-alpha"),
        aggregate_type: "task".into(),
        aggregate_id: AggregateId::new("task-0001"),
        aggregate_version: 1,
        event_type: "task_created".into(),
        schema_version: 1,
        global_sequence: Seq::new(1),
        occurred_at: Timestamp::new("2026-07-27T12:00:00Z"),
        appended_at: Timestamp::new("2026-07-27T12:00:00Z"),
        actor: actor("operator", None),
        origin: Origin {
            system: "cli".into(),
            r#ref: None,
        },
        causation_id: None,
        correlation_id: None,
        idempotency_key: None,
        payload: serde_json::json!({}),
        payload_ref: None,
    }
}

fn golden_command_envelope() -> CommandEnvelope {
    CommandEnvelope {
        command_id: CommandId::new("cmd-0001"),
        project_id: ProjectId::new("proj-alpha"),
        command_type: "cancel_attempt".into(),
        schema_version: 1,
        issued_at: Timestamp::new("2026-07-27T12:05:00Z"),
        actor: actor("operator", Some("op-1")),
        origin: Origin {
            system: "cli".into(),
            r#ref: None,
        },
        target_aggregate_type: Some("attempt".into()),
        target_aggregate_id: Some(AggregateId::new("att-0001")),
        expected_version: Some(4),
        idempotency_key: IdempotencyKey::new("cancel-once"),
        causation_id: None,
        correlation_id: Some(CorrelationId::new("corr-7")),
        payload: serde_json::json!({ "reason": "operator_requested" }),
    }
}

fn golden_task() -> Task {
    Task {
        id: TaskId::new("task-0001"),
        version: 3,
        state: TaskState::InputRequired,
        kind: Some("execution".into()),
        title: Some("ship the contract".into()),
        spec_ref: Some("spec://proj-alpha/contract".into()),
        project: Some("proj-alpha".into()),
        priority: Some(2),
        tracker_ref: Some("tracker://issue/42".into()),
        created_at: Timestamp::new("2026-07-27T10:00:00Z"),
        updated_at: Timestamp::new("2026-07-27T12:00:00Z"),
    }
}

fn golden_attempt() -> Attempt {
    Attempt {
        id: AttemptId::new("att-0001"),
        version: 5,
        state: AttemptState::Blocked,
        task_id: TaskId::new("task-0001"),
        engine: EngineId::new("engine-a"),
        capability: Some("code_write".into()),
        role: Some("implementer".into()),
        model_lane: Some("standard".into()),
        permission_profile: Some("workspace_write".into()),
        worktree_lease_id: Some(LeaseId::new("lease-0001")),
        base_sha: Some("0123456789abcdef0123456789abcdef01234567".into()),
        budget: Some(Budget {
            max_tokens: Some(2_000_000),
            max_tool_calls: Some(150),
            max_wall_ms: Some(2_400_000),
            max_cost_micros: Some(CostMicros::new(5_000_000)),
        }),
        provider_session_ref: Some("sess-9".into()),
        runtime_ref: Some("pid:4242".into()),
        runtime_started_at: Some(Timestamp::new("2026-07-27T11:00:00Z")),
        exit_code: None,
        provider_terminal_event: None,
        result_valid: None,
        evidence_manifest_ref: None,
        created_at: Timestamp::new("2026-07-27T10:30:00Z"),
        updated_at: Timestamp::new("2026-07-27T12:00:00Z"),
    }
}

fn golden_message() -> Message {
    let mut delivery_refs = BTreeMap::new();
    delivery_refs.insert("chat".to_string(), "chat-msg-77".to_string());
    delivery_refs.insert("inbox".to_string(), "inbox-3".to_string());
    Message {
        id: MessageId::new("msg-0001"),
        version: 4,
        state: MessageState::Applied,
        idempotency_key: IdempotencyKey::new("send-once"),
        correlation_id: Some(CorrelationId::new("corr-7")),
        reply_to: None,
        sender: Some("orchestrator".into()),
        recipient: Some("operator".into()),
        channel: Some("chat".into()),
        kind: Some("status_update".into()),
        payload: Some(serde_json::json!({ "text": "verify passed" })),
        deadline: None,
        delivery_attempts: 1,
        dead_letter_reason: None,
        delivery_refs: Some(delivery_refs),
        created_at: Timestamp::new("2026-07-27T12:00:00Z"),
        updated_at: Timestamp::new("2026-07-27T12:01:00Z"),
    }
}

fn golden_command_terminal() -> Command {
    Command {
        id: CommandId::new("cmd-0001"),
        version: 4,
        state: CommandState::VerificationComplete,
        kind: "stop_attempt".into(),
        target: Some("att-0001".into()),
        actor: Some(actor("operator", Some("op-1"))),
        idempotency_key: Some(IdempotencyKey::new("cancel-once")),
        outcome: Some(Outcome::Clean),
        created_at: Timestamp::new("2026-07-27T12:05:00Z"),
        updated_at: Timestamp::new("2026-07-27T12:06:00Z"),
    }
}

fn golden_workspace_node() -> WorkspaceNode {
    WorkspaceNode {
        id: WorkspaceNodeId::new("pane-0001"),
        version: 3,
        kind: WorkspaceNodeKind::Pane,
        parent_id: Some(WorkspaceNodeId::new("tab-0001")),
        session_id: Some(PtySessionId::new("pty-1")),
        created_at: Timestamp::new("2026-08-09T12:00:00Z"),
        updated_at: Timestamp::new("2026-08-09T12:05:00Z"),
    }
}

fn golden_workflow_run() -> WorkflowRun {
    WorkflowRun {
        id: WorkflowRunId::new("wfr-0001"),
        version: 4,
        state: "completed".to_owned(),
        step: Some("ship".to_owned()),
        template_ref: "seven-act@1".to_owned(),
        template_sha256: Some(
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08".to_owned(),
        ),
        task_id: Some(TaskId::new("task-0001")),
        title: Some("wire the replay lens".to_owned()),
        opened_at: Timestamp::new("2026-08-09T12:00:00Z"),
        updated_at: Timestamp::new("2026-08-09T13:00:00Z"),
        closed_at: Some(Timestamp::new("2026-08-09T13:00:00Z")),
    }
}

fn golden_pty_session() -> PtySession {
    PtySession {
        id: PtySessionId::new("pty-0001"),
        version: 5,
        state: "closed".to_owned(),
        generation: PtySessionGeneration::new("gen-0001"),
        engine_session_id: Some(EngineSessionId::new("session-0001")),
        attach_count: 3,
        detach_count: 3,
        title: Some("the daily driver".to_owned()),
        opened_at: Timestamp::new("2026-08-09T12:00:00Z"),
        updated_at: Timestamp::new("2026-08-09T18:00:00Z"),
        closed_at: Some(Timestamp::new("2026-08-09T18:00:00Z")),
    }
}

fn golden_pty_session_template() -> PtySessionTemplate {
    PtySessionTemplate {
        name: PtySessionTemplateName::new("review"),
        version: 1,
        state: "active".to_owned(),
        command: "/usr/bin/review-agent".to_owned(),
        args: vec!["--interactive".to_owned()],
        cwd: Some("/work/gridwork".to_owned()),
        env: BTreeMap::from([("TERM".to_owned(), "env:TERM".to_owned())]),
        cols: 100,
        rows: 30,
        declared_at: Timestamp::new("2026-08-12T12:00:00Z"),
        updated_at: Timestamp::new("2026-08-12T12:00:00Z"),
        retired_at: None,
    }
}

fn golden_transition_results() -> Vec<TransitionResult<TaskState>> {
    vec![
        TransitionResult::Applied {
            state: TaskState::Working,
            version: 2,
        },
        TransitionResult::IllegalEdge {
            from: TaskState::Completed,
            to: TaskState::Working,
        },
        TransitionResult::StaleVersion {
            actual: 3,
            expected: 2,
        },
        TransitionResult::UnauthorizedActor {
            reason: "edge requires actor kind liveness_producer".into(),
        },
    ]
}

fn golden_checkpoint() -> OrchestratorCheckpoint {
    OrchestratorCheckpoint {
        orchestrator_id: Some("orch-1".into()),
        seq: Seq::new(42),
        native_session_ref: None,
        active_goal: Some("contract phase".into()),
        active_step_ref: Some("step://execute".into()),
        latest_command_ref: None,
        open_attempts: Some(vec![]),
        leases: None,
        pending_approvals: Some(vec![PendingApproval {
            kind: "design_fork".into(),
            question: "which storage layout?".into(),
            subject_ref: Some("task-0001".into()),
            raised_at: Timestamp::new("2026-07-27T12:00:00Z"),
        }]),
        budget_cursor: Some(BudgetCursor {
            spent_tokens: Some(120_000),
            spent_tool_calls: Some(14),
            spent_cost_micros: None,
            window_started_at: None,
        }),
    }
}

fn capability(name: &str) -> CapabilityName {
    CapabilityName::new(name).expect("golden capability name is valid")
}

fn blob_address(nibble: char) -> BlobAddress {
    BlobAddress::from_digest(&nibble.to_string().repeat(64)).expect("golden digest is valid")
}

fn golden_activate_command() -> KernelCommand {
    KernelCommand::ActivateKernel {
        cutover_id: "00000000-0000-4000-8000-000000000001".into(),
        archive_manifest_sha256: "b".repeat(64),
    }
}

/// The activation envelope, exactly as `gw kernel activate` submits it: the
/// typed command IS the payload, and `command_type` names the same variant.
fn golden_activate_envelope() -> CommandEnvelope {
    CommandEnvelope {
        command_id: CommandId::new("cmd-activate"),
        project_id: ProjectId::new("system"),
        command_type: "activate_kernel".into(),
        schema_version: 1,
        issued_at: Timestamp::new("2026-07-28T12:00:00Z"),
        actor: actor("operator", Some("op-1")),
        origin: Origin {
            system: "gw".into(),
            r#ref: None,
        },
        target_aggregate_type: Some("kernel".into()),
        target_aggregate_id: Some(AggregateId::new("singleton")),
        expected_version: Some(1),
        idempotency_key: IdempotencyKey::new(
            "kernel_activated:00000000-0000-4000-8000-000000000001",
        ),
        causation_id: None,
        correlation_id: None,
        payload: serde_json::to_value(golden_activate_command()).expect("serialize command"),
    }
}

fn pty_session_id() -> PtySessionId {
    PtySessionId::new("pty-1")
}

fn pty_generation() -> PtySessionGeneration {
    PtySessionGeneration::new("pty-life-1")
}

fn golden_pty_input_envelope() -> CommandEnvelope {
    let command = KernelCommand::SendPtyInput {
        pty_session_id: pty_session_id(),
        generation: pty_generation(),
        byte_count: ByteCount::new(3),
    };
    CommandEnvelope {
        command_id: CommandId::new("input-1"),
        project_id: ProjectId::new("proj-alpha"),
        command_type: command.command_type().into(),
        schema_version: 1,
        issued_at: Timestamp::new("2026-08-11T12:00:00Z"),
        actor: actor("operator", Some("op-1")),
        origin: Origin {
            system: "gw".into(),
            r#ref: None,
        },
        target_aggregate_type: None,
        target_aggregate_id: None,
        expected_version: None,
        idempotency_key: IdempotencyKey::new("input-1"),
        causation_id: None,
        correlation_id: None,
        payload: serde_json::to_value(command).expect("serialize input command"),
    }
}

fn golden_pty_control_envelope(command: KernelCommand) -> CommandEnvelope {
    let command_type = command.command_type();
    CommandEnvelope {
        command_id: CommandId::new(format!("{command_type}-1")),
        project_id: ProjectId::new("system"),
        command_type: command_type.into(),
        schema_version: 1,
        issued_at: Timestamp::new("2026-08-12T12:00:00Z"),
        actor: actor("operator", Some("op-1")),
        origin: Origin {
            system: "gw".into(),
            r#ref: None,
        },
        target_aggregate_type: None,
        target_aggregate_id: None,
        expected_version: None,
        idempotency_key: IdempotencyKey::new(format!("{command_type}-1")),
        causation_id: None,
        correlation_id: None,
        payload: serde_json::to_value(command).expect("serialize PTY control command"),
    }
}

/// The SGR-0 rendition: no colors, no underline, every flag off.
fn plain_style() -> CellStyle {
    CellStyle {
        bold: false,
        dim: false,
        italic: false,
        blink: false,
        inverse: false,
        invisible: false,
        strikethrough: false,
        overline: false,
        underline: None,
        fg: None,
        bg: None,
        underline_color: None,
    }
}

/// A 2x3 interned frame exercising every color tier, every attribute in its
/// non-default state spread across the style table, both run kinds, an
/// empty-glyph cell (a wide cluster's trailing half), a table index reused
/// across rows, and the terminal-default (no `fg`/`bg`) style.
///
/// Spreading the attributes rather than setting them all on one style is
/// deliberate: a golden where every flag is `true` cannot catch two fields
/// swapped in the generated bindings, because both sides read the same.
fn golden_pty_frame() -> PtyFrame {
    PtyFrame {
        styles: vec![
            plain_style(),
            CellStyle {
                bold: true,
                blink: true,
                fg: Some(CellColor::Ansi16 {
                    slot: PtyAnsiSlot::BrightCyan,
                }),
                ..plain_style()
            },
            CellStyle {
                dim: true,
                italic: true,
                // Curly, not Single: a `4:3` underline is the case the
                // old `bool` reported as indistinguishable from `4`.
                underline: Some(CellUnderline::Curly),
                underline_color: Some(CellColor::Ansi16 {
                    slot: PtyAnsiSlot::Red,
                }),
                fg: Some(CellColor::Truecolor {
                    r: 0xff,
                    g: 0x00,
                    b: 0x80,
                }),
                bg: Some(CellColor::Xterm256 { index: 236 }),
                ..plain_style()
            },
            CellStyle {
                inverse: true,
                invisible: true,
                strikethrough: true,
                overline: true,
                ..plain_style()
            },
        ],
        rows: vec![
            vec![
                PtyRun::Cells {
                    style: 0,
                    glyphs: vec!["g".into(), "w".into()],
                },
                PtyRun::Fill {
                    style: 1,
                    glyph: " ".into(),
                    count: 1,
                },
            ],
            vec![
                PtyRun::Cells {
                    style: 2,
                    glyphs: vec!["k".into(), String::new()],
                },
                PtyRun::Fill {
                    style: 3,
                    glyph: " ".into(),
                    count: 1,
                },
            ],
        ],
        cursor: Some(PtyCursor { row: 1, col: 1 }),
    }
}

fn golden_pty_deltas() -> Vec<PtyDelta> {
    vec![
        PtyDelta::CellsChanged {
            styles: vec![plain_style()],
            updates: vec![
                PtyCellUpdate {
                    row: 0,
                    col: 0,
                    glyph: "G".into(),
                    style: 0,
                },
                PtyCellUpdate {
                    row: 0,
                    col: 1,
                    glyph: "W".into(),
                    style: 0,
                },
            ],
        },
        PtyDelta::Resized { rows: 24, cols: 80 },
    ]
}

fn golden_client_control() -> Vec<ClientControl> {
    vec![
        ClientControl::Hello {
            protocol_major: ProtocolVersion::V1,
            protocol_minor: PROTOCOL_MINOR,
            capabilities: vec![
                capability("event_subscribe"),
                capability("blob"),
                capability("pty_raw"),
                capability(gwk_domain::PTY_INPUT_CAPABILITY),
                capability("pty_control"),
                capability("pty_start"),
            ],
            client: Some("gw".into()),
        },
        ClientControl::Request {
            request_id: RequestId::new("req-1"),
            request: KernelRequest::VerifySealed {},
        },
        ClientControl::Request {
            request_id: RequestId::new("req-2"),
            request: KernelRequest::SubmitCommand {
                envelope: golden_activate_envelope(),
            },
        },
        ClientControl::Request {
            request_id: RequestId::new("req-3"),
            request: KernelRequest::ReadEvents {
                cursor: Some(Seq::new(9_007_199_254_740_993)),
                limit: 512,
            },
        },
        ClientControl::Request {
            request_id: RequestId::new("req-4"),
            request: KernelRequest::ListProjection {
                projection: ProjectionKind::Attempt,
                cursor: None,
                limit: Some(50),
            },
        },
        ClientControl::Request {
            request_id: RequestId::new("req-5"),
            request: KernelRequest::BlobCommit {
                upload_id: BlobUploadId::new("upl-1"),
                address: blob_address('a'),
            },
        },
        // A reattach: `cursor` present, resuming after a prior frame revision.
        ClientControl::Request {
            request_id: RequestId::new("req-6"),
            request: KernelRequest::PtyAttach {
                session_id: pty_session_id(),
                generation: Some(pty_generation()),
                cursor: Some(PtyFrameSeq::new(42)),
            },
        },
        ClientControl::Request {
            request_id: RequestId::new("req-7"),
            request: KernelRequest::PtySnapshot {
                session_id: pty_session_id(),
            },
        },
        // The host's half of the PTY surface: a claim/reseed, one delta
        // batch, and the explicit retire.
        ClientControl::Request {
            request_id: RequestId::new("req-8"),
            request: KernelRequest::PtyPublishSnapshot {
                session_id: pty_session_id(),
                seq: Some(PtyFrameSeq::new(43)),
                frame: golden_pty_frame(),
            },
        },
        ClientControl::Request {
            request_id: RequestId::new("req-9"),
            request: KernelRequest::PtyPublishDeltas {
                session_id: pty_session_id(),
                seq: PtyFrameSeq::new(44),
                deltas: golden_pty_deltas(),
            },
        },
        ClientControl::Request {
            request_id: RequestId::new("req-10"),
            request: KernelRequest::PtyRetire {
                session_id: pty_session_id(),
            },
        },
        ClientControl::Request {
            request_id: RequestId::new("req-11"),
            request: KernelRequest::PtyRawAttach {
                session_id: pty_session_id(),
                generation: Some(pty_generation()),
                cursor: Some(PtyFrameSeq::new(44)),
            },
        },
        ClientControl::PtyRawPublishSnapshot {
            request_id: RequestId::new("req-12"),
            session_id: pty_session_id(),
            seq: None,
            rows: 24,
            cols: 80,
            byte_size: ByteCount::new(1_024),
        },
        ClientControl::PtyRawPublishOutput {
            request_id: RequestId::new("req-13"),
            session_id: pty_session_id(),
            seq: PtyFrameSeq::new(0),
            byte_size: ByteCount::new(7),
        },
        ClientControl::Request {
            request_id: RequestId::new("req-14"),
            request: KernelRequest::PtyPublishRawResize {
                session_id: pty_session_id(),
                seq: PtyFrameSeq::new(1),
                rows: 30,
                cols: 100,
            },
        },
        ClientControl::Request {
            request_id: RequestId::new("req-15"),
            request: KernelRequest::SendPtyInput {
                envelope: golden_pty_input_envelope(),
                data_base64: PtyInputData::new("AP8K"),
            },
        },
        ClientControl::Request {
            request_id: RequestId::new("req-16"),
            request: KernelRequest::ResizePtySession {
                envelope: golden_pty_control_envelope(KernelCommand::ResizePtySession {
                    pty_session_id: pty_session_id(),
                    generation: pty_generation(),
                    cols: 120,
                    rows: 40,
                }),
            },
        },
        ClientControl::Request {
            request_id: RequestId::new("req-17"),
            request: KernelRequest::StopPtySession {
                envelope: golden_pty_control_envelope(KernelCommand::StopPtySession {
                    pty_session_id: pty_session_id(),
                    generation: pty_generation(),
                }),
            },
        },
        ClientControl::Request {
            request_id: RequestId::new("req-18"),
            request: KernelRequest::StartPtySession {
                envelope: golden_pty_control_envelope(KernelCommand::RequestPtySessionStart {
                    template_name: PtySessionTemplateName::new("review"),
                    pty_session_id: PtySessionId::new("review-7"),
                }),
            },
        },
        ClientControl::PtyDeliveryAck {
            delivery_id: EventId::new("pty_session:pty-1:2"),
            result: gwk_domain::protocol::PtyDeliveryResult::Applied,
        },
    ]
}

fn golden_server_control() -> Vec<ServerControl> {
    vec![
        ServerControl::HelloAck {
            protocol_major: ProtocolVersion::V1,
            protocol_minor: PROTOCOL_MINOR,
            // The INTERSECTION: the client asked for three capabilities; this
            // kernel offers only the raw PTY fallback.
            capabilities: vec![
                capability("pty_raw"),
                capability(gwk_domain::PTY_INPUT_CAPABILITY),
                capability("pty_control"),
                capability("pty_start"),
            ],
            sealed: true,
            watermark: Some(Seq::new(9_007_199_254_740_993)),
        },
        ServerControl::HelloRefusal {
            code: KernelErrorCode::UnsupportedVersion,
            message: "protocol major 2 is not served".into(),
        },
        ServerControl::Response {
            request_id: RequestId::new("req-1"),
            result: KernelResult::SealedVerification {
                sealed: true,
                genesis_event_id: EventId::new("evt-genesis"),
                // NOT 1: the sequence is database-assigned.
                genesis_watermark: Seq::new(9_007_199_254_740_993),
                event_count: EventCount::new(1),
            },
        },
        ServerControl::Response {
            request_id: RequestId::new("req-2"),
            result: KernelResult::Error {
                code: KernelErrorCode::IdempotencyConflict,
                message: "idempotency key reused with different request content".into(),
                detail: Some(serde_json::json!({ "idempotency_key": "kernel_activated:cut-1" })),
            },
        },
        ServerControl::Response {
            request_id: RequestId::new("req-5"),
            result: KernelResult::BlobCommitted {
                descriptor: BlobDescriptor {
                    address: blob_address('a'),
                    media_type: "application/octet-stream".into(),
                    byte_size: ByteCount::new(1_048_576),
                    kek_id: "kek-2026-07".into(),
                    created_at: Timestamp::new("2026-07-28T12:00:02Z"),
                    pinned: true,
                    tombstoned: false,
                },
                deduplicated: false,
            },
        },
        ServerControl::EventBatch {
            request_id: RequestId::new("req-6"),
            events: vec![golden_event_envelope_minimal()],
            cursor: Seq::new(1),
        },
        ServerControl::StreamClosed {
            request_id: RequestId::new("req-6"),
            code: KernelErrorCode::SlowConsumer,
            // The consumer resumes from here rather than restarting.
            last_cursor: Some(Seq::new(1)),
        },
        ServerControl::Response {
            request_id: RequestId::new("req-6"),
            result: KernelResult::PtyAttached {
                session_id: pty_session_id(),
                generation: pty_generation(),
                rows: 24,
                cols: 80,
                cursor: Some(PtyFrameSeq::new(42)),
            },
        },
        ServerControl::Response {
            request_id: RequestId::new("req-7"),
            result: KernelResult::PtySnapshot {
                session_id: pty_session_id(),
                generation: pty_generation(),
                seq: PtyFrameSeq::new(43),
                frame: golden_pty_frame(),
            },
        },
        // Pushed on the PtyAttach's own request_id, mirroring how EventBatch
        // and StreamClosed above both ride the SubscribeEvents that opened
        // them.
        ServerControl::PtyDeltaBatch {
            request_id: RequestId::new("req-6"),
            session_id: pty_session_id(),
            generation: pty_generation(),
            deltas: golden_pty_deltas(),
            seq: PtyFrameSeq::new(44),
        },
        ServerControl::PtyStreamClosed {
            request_id: RequestId::new("req-6"),
            generation: pty_generation(),
            code: KernelErrorCode::SlowConsumer,
            last_seq: Some(PtyFrameSeq::new(44)),
        },
        ServerControl::Response {
            request_id: RequestId::new("req-9"),
            result: KernelResult::PtyPublished {
                session_id: pty_session_id(),
            },
        },
        ServerControl::Response {
            request_id: RequestId::new("req-10"),
            result: KernelResult::PtyRetired {
                session_id: pty_session_id(),
            },
        },
        ServerControl::Response {
            request_id: RequestId::new("req-11"),
            result: KernelResult::PtyRawAttached {
                session_id: pty_session_id(),
                generation: pty_generation(),
                rows: 24,
                cols: 80,
                cursor: None,
            },
        },
        ServerControl::PtyRawSnapshot {
            request_id: RequestId::new("req-11"),
            generation: pty_generation(),
            seq: None,
            rows: 24,
            cols: 80,
            byte_size: ByteCount::new(1_024),
        },
        ServerControl::PtyRawChunk {
            request_id: RequestId::new("req-11"),
            generation: pty_generation(),
            seq: PtyFrameSeq::new(0),
            byte_size: ByteCount::new(7),
        },
        ServerControl::PtyRawResized {
            request_id: RequestId::new("req-11"),
            generation: pty_generation(),
            seq: PtyFrameSeq::new(1),
            rows: 30,
            cols: 100,
        },
        ServerControl::PtyRawStreamClosed {
            request_id: RequestId::new("req-11"),
            generation: pty_generation(),
            code: KernelErrorCode::SlowConsumer,
            last_seq: Some(PtyFrameSeq::new(1)),
        },
        ServerControl::Response {
            request_id: RequestId::new("req-13"),
            result: KernelResult::PtyPublished {
                session_id: pty_session_id(),
            },
        },
        ServerControl::PtyInput {
            delivery_id: EventId::new("pty_session:pty-1:2"),
            command_id: CommandId::new("input-1"),
            session_id: pty_session_id(),
            generation: pty_generation(),
            byte_size: ByteCount::new(3),
            data_base64: PtyInputData::new("AP8K"),
        },
        ServerControl::PtyResize {
            delivery_id: EventId::new("pty_session:pty-1:3"),
            command_id: CommandId::new("resize-1"),
            session_id: pty_session_id(),
            generation: pty_generation(),
            cols: 120,
            rows: 40,
        },
        ServerControl::PtyStop {
            delivery_id: EventId::new("pty_session:pty-1:4"),
            command_id: CommandId::new("stop-1"),
            session_id: pty_session_id(),
            generation: pty_generation(),
        },
        ServerControl::PtyStart {
            delivery_id: EventId::new("pty_session_start:review-7:1"),
            command_id: CommandId::new("start-1"),
            template_name: PtySessionTemplateName::new("review"),
            session_id: PtySessionId::new("review-7"),
        },
        ServerControl::PtyDeliverySettled {
            delivery_id: EventId::new("pty_session:pty-1:2"),
        },
    ]
}

fn golden_kernel_checkpoint() -> Checkpoint {
    Checkpoint {
        schema_version: CHECKPOINT_SCHEMA_VERSION,
        through_sequence: Seq::new(9_007_199_254_740_993),
        projection_hash: "c".repeat(64),
        records_ref: PayloadRef {
            digest: format!("sha256:{}", "d".repeat(64)),
            media_type: "application/octet-stream".into(),
            byte_size: ByteCount::new(4_194_304),
            retention_class: Some("standard".into()),
            evidence_pin: None,
        },
        created_at: Timestamp::new("2026-07-28T12:00:03Z"),
    }
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct RawWirePairGolden {
    wire_hex: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct RawWireGolden {
    publish: RawWirePairGolden,
    delivery: RawWirePairGolden,
}

fn framed_bytes(kind: FrameKind, body: &[u8]) -> Vec<u8> {
    let body_length = u32::try_from(body.len() + 1).expect("golden frame length fits u32");
    let mut wire = Vec::with_capacity(4 + body_length as usize);
    wire.extend_from_slice(&body_length.to_be_bytes());
    wire.push(kind.as_u8());
    wire.extend_from_slice(body);
    wire
}

fn framed_pair<T: serde::Serialize>(header: &T, payload: &[u8]) -> RawWirePairGolden {
    let json = serde_json::to_vec(header).expect("serialize raw wire header");
    let mut wire = framed_bytes(FrameKind::Json, &json);
    wire.extend(framed_bytes(FrameKind::PtyRaw, payload));
    let mut wire_hex = String::with_capacity(wire.len() * 2);
    for byte in wire {
        write!(&mut wire_hex, "{byte:02x}").expect("write hex to string");
    }
    RawWirePairGolden { wire_hex }
}

fn golden_raw_wire() -> RawWireGolden {
    let payload = [0x00, 0xff, 0x1b, b'[', 0xc3];
    RawWireGolden {
        publish: framed_pair(
            &ClientControl::PtyRawPublishOutput {
                request_id: RequestId::new("raw-wire-publish"),
                session_id: pty_session_id(),
                seq: PtyFrameSeq::new(7),
                byte_size: ByteCount::new(payload.len() as u64),
            },
            &payload,
        ),
        delivery: framed_pair(
            &ServerControl::PtyRawChunk {
                request_id: RequestId::new("raw-wire-attach"),
                generation: pty_generation(),
                seq: PtyFrameSeq::new(7),
                byte_size: ByteCount::new(payload.len() as u64),
            },
            &payload,
        ),
    }
}

fn goldens() -> Vec<(&'static str, String)> {
    fn pretty<T: serde::Serialize>(value: &T) -> String {
        let mut out = serde_json::to_string_pretty(value).expect("serialize golden");
        out.push('\n');
        out
    }
    vec![
        (
            "event-envelope-full.json",
            pretty(&golden_event_envelope_full()),
        ),
        (
            "event-envelope-minimal.json",
            pretty(&golden_event_envelope_minimal()),
        ),
        (
            "context-resolved-manifest.json",
            pretty(&golden_resolved_manifest()),
        ),
        (
            "context-release-supplement.json",
            pretty(&golden_release_supplement()),
        ),
        (
            "context-observation-supplement.json",
            pretty(&golden_observation_supplement()),
        ),
        (
            "context-finalization-supplement.json",
            pretty(&golden_finalization_supplement()),
        ),
        ("command-envelope.json", pretty(&golden_command_envelope())),
        ("task.json", pretty(&golden_task())),
        ("attempt.json", pretty(&golden_attempt())),
        ("message.json", pretty(&golden_message())),
        (
            "command-verification-complete.json",
            pretty(&golden_command_terminal()),
        ),
        ("workspace-node.json", pretty(&golden_workspace_node())),
        ("workflow-run.json", pretty(&golden_workflow_run())),
        ("pty-session.json", pretty(&golden_pty_session())),
        (
            "pty-session-template.json",
            pretty(&golden_pty_session_template()),
        ),
        (
            "transition-results.json",
            pretty(&golden_transition_results()),
        ),
        ("orchestrator-checkpoint.json", pretty(&golden_checkpoint())),
        (
            "kernel-client-control.json",
            pretty(&golden_client_control()),
        ),
        (
            "kernel-server-control.json",
            pretty(&golden_server_control()),
        ),
        ("pty-raw-wire.json", pretty(&golden_raw_wire())),
        (
            "kernel-checkpoint.json",
            pretty(&golden_kernel_checkpoint()),
        ),
    ]
}

/// Decode a TS-re-emitted golden back into `T` and require value equality
/// with the Rust fixture.
fn round_trip<T: serde::de::DeserializeOwned + serde::Serialize>(
    name: &str,
    ts_dir: &Path,
    rust_value_json: &str,
) -> Result<(), String> {
    let path = ts_dir.join(name);
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("{}: unreadable TS-emitted golden: {e}", path.display()))?;
    let decoded: T = serde_json::from_str(&raw).map_err(|e| {
        format!("{name}: TS-emitted golden does not decode into the Rust type: {e}")
    })?;
    let canonical = serde_json::to_value(&decoded).expect("re-serialize decoded value");
    let original: serde_json::Value =
        serde_json::from_str(rust_value_json).expect("parse rust golden");
    if canonical != original {
        return Err(format!("{name}: TS round-trip changed the value"));
    }
    Ok(())
}

fn check_file(path: &Path, expected: &str, drift: &mut Vec<String>) {
    match std::fs::read_to_string(path) {
        Ok(actual) if actual == expected => {}
        Ok(_) => drift.push(format!("{}: drifted from generated output", path.display())),
        Err(_) => drift.push(format!(
            "{}: missing (run `cargo run -p xtask -- contract`)",
            path.display()
        )),
    }
}

/// Fail on a `.json` file present in `dir` that isn't one of `expected` — a
/// golden renamed or dropped in the generator otherwise leaves an orphan on
/// disk that `check_file` never looks at and no gate notices.
fn check_orphans(dir: &Path, expected: &[&str], drift: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return; // a missing dir is already reported via check_file on each expected path
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !expected.contains(&name) {
            drift.push(format!(
                "{}: orphaned golden — not produced by `cargo run -p xtask -- contract` \
                 (remove it, or register its type if it should still be generated)",
                path.display()
            ));
        }
    }
}

pub fn run(check: bool) {
    let root = repo_root();
    let contracts = root.join("contracts");
    let goldens_dir = contracts.join("goldens");
    let ts_goldens_dir = contracts.join("goldens-ts");

    let bindings_ts = bindings();
    let theme_json = signal_theme_json();
    let golden_files = goldens();

    let contract_sql_path = root.join(crate::schema::GENERATED_PATH);
    let contract_sql = std::fs::read_to_string(root.join("schema/0001_contract.sql"))
        .expect("read schema/0001_contract.sql");
    inspect_context_truth_schema(&contract_sql).unwrap_or_else(|message| panic!("{message}"));
    let contract_sql_rs = crate::schema::contract_sql_rs(&contract_sql);

    if !check {
        std::fs::create_dir_all(&goldens_dir).expect("create contracts/goldens");
        std::fs::write(contracts.join("bindings.ts"), &bindings_ts).expect("write bindings.ts");
        std::fs::write(contracts.join("signal-theme.json"), &theme_json)
            .expect("write signal-theme.json");
        for (name, content) in &golden_files {
            std::fs::write(goldens_dir.join(name), content).expect("write golden");
        }
        std::fs::write(&contract_sql_path, &contract_sql_rs).expect("write contract_sql.rs");
        eprintln!(
            "contract: wrote bindings.ts, signal-theme.json, {} goldens, {}",
            golden_files.len(),
            crate::schema::GENERATED_PATH
        );
        return;
    }

    let mut drift: Vec<String> = Vec::new();
    check_file(&contracts.join("bindings.ts"), &bindings_ts, &mut drift);
    check_file(&contract_sql_path, &contract_sql_rs, &mut drift);
    check_file(
        &contracts.join("signal-theme.json"),
        &theme_json,
        &mut drift,
    );
    let golden_names: Vec<&str> = golden_files.iter().map(|(name, _)| *name).collect();
    for (name, content) in &golden_files {
        check_file(&goldens_dir.join(name), content, &mut drift);
    }
    check_orphans(&goldens_dir, &golden_names, &mut drift);
    check_orphans(&ts_goldens_dir, &golden_names, &mut drift);

    // TS -> Rust half of the round trip (the bun tests re-emit what they
    // decoded; those files are committed and re-read here).
    let find = |name: &str| -> &str {
        &golden_files
            .iter()
            .find(|(n, _)| *n == name)
            .expect("known golden")
            .1
    };
    let round_trips = [
        round_trip::<EventEnvelope>(
            "event-envelope-full.json",
            &ts_goldens_dir,
            find("event-envelope-full.json"),
        ),
        round_trip::<EventEnvelope>(
            "event-envelope-minimal.json",
            &ts_goldens_dir,
            find("event-envelope-minimal.json"),
        ),
        round_trip::<ResolvedManifest>(
            "context-resolved-manifest.json",
            &ts_goldens_dir,
            find("context-resolved-manifest.json"),
        ),
        round_trip::<ReleaseSupplement>(
            "context-release-supplement.json",
            &ts_goldens_dir,
            find("context-release-supplement.json"),
        ),
        round_trip::<ObservationSupplement>(
            "context-observation-supplement.json",
            &ts_goldens_dir,
            find("context-observation-supplement.json"),
        ),
        round_trip::<FinalizationSupplement>(
            "context-finalization-supplement.json",
            &ts_goldens_dir,
            find("context-finalization-supplement.json"),
        ),
        round_trip::<CommandEnvelope>(
            "command-envelope.json",
            &ts_goldens_dir,
            find("command-envelope.json"),
        ),
        round_trip::<Task>("task.json", &ts_goldens_dir, find("task.json")),
        round_trip::<Attempt>("attempt.json", &ts_goldens_dir, find("attempt.json")),
        round_trip::<Message>("message.json", &ts_goldens_dir, find("message.json")),
        round_trip::<Command>(
            "command-verification-complete.json",
            &ts_goldens_dir,
            find("command-verification-complete.json"),
        ),
        round_trip::<WorkspaceNode>(
            "workspace-node.json",
            &ts_goldens_dir,
            find("workspace-node.json"),
        ),
        round_trip::<WorkflowRun>(
            "workflow-run.json",
            &ts_goldens_dir,
            find("workflow-run.json"),
        ),
        round_trip::<PtySession>(
            "pty-session.json",
            &ts_goldens_dir,
            find("pty-session.json"),
        ),
        round_trip::<PtySessionTemplate>(
            "pty-session-template.json",
            &ts_goldens_dir,
            find("pty-session-template.json"),
        ),
        round_trip::<Vec<TransitionResult<TaskState>>>(
            "transition-results.json",
            &ts_goldens_dir,
            find("transition-results.json"),
        ),
        round_trip::<OrchestratorCheckpoint>(
            "orchestrator-checkpoint.json",
            &ts_goldens_dir,
            find("orchestrator-checkpoint.json"),
        ),
        round_trip::<Vec<ClientControl>>(
            "kernel-client-control.json",
            &ts_goldens_dir,
            find("kernel-client-control.json"),
        ),
        round_trip::<Vec<ServerControl>>(
            "kernel-server-control.json",
            &ts_goldens_dir,
            find("kernel-server-control.json"),
        ),
        round_trip::<RawWireGolden>(
            "pty-raw-wire.json",
            &ts_goldens_dir,
            find("pty-raw-wire.json"),
        ),
        round_trip::<Checkpoint>(
            "kernel-checkpoint.json",
            &ts_goldens_dir,
            find("kernel-checkpoint.json"),
        ),
    ];
    for result in round_trips {
        if let Err(msg) = result {
            drift.push(msg);
        }
    }

    if drift.is_empty() {
        eprintln!("contract: check clean");
    } else {
        for line in &drift {
            eprintln!("contract drift: {line}");
        }
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pinned count of `.register::<T>()` calls inside `bindings()` — the
    /// manual root registry described in its doc comment. Update this
    /// constant AND the `.register()` chain together in the same change;
    /// a mismatch means one moved without the other.
    const REGISTERED_ROOT_COUNT: usize = 46;

    #[test]
    fn bindings_registry_matches_its_pin() {
        // Scoped to the `bindings()` function, not the whole file — this doc
        // comment and this very test both say `.register::<T>()` in prose, and
        // a whole-file scan would count its own words. The scope runs from the
        // signature to the function's terminating column-0 `}` (the `"\n}\n"`
        // guaranteed by the `cargo fmt --check` gate). No brace-depth counting,
        // so a stray `}` inside a string or comment in the body — a future
        // `format!("... }}")`, a lone `}` in a literal — can't underflow a
        // usize (panic) or mis-scope: it is indented, never column-0.
        let body = bindings_body(include_str!("contract.rs")).expect("bindings body present");
        let actual = body.matches(".register::<").count();
        assert_eq!(
            actual, REGISTERED_ROOT_COUNT,
            "bindings() now has {actual} `.register::<T>()` calls but \
             REGISTERED_ROOT_COUNT pins {REGISTERED_ROOT_COUNT} — update BOTH \
             the registry in bindings() and this constant, and confirm the \
             type you added or removed actually needed a manual root (see \
             bindings()'s doc comment)"
        );
    }

    #[test]
    fn the_attribution_guard_refuses_a_client_submittable_actor_field() {
        // The guard this replaces swept a hand-written list of fact instances,
        // and an eleventh variant carrying an actor — mapped onto an existing
        // event name — passed it with every test green. This one reads the
        // generated surface, so the eleventh variant is covered the day it
        // exists without anyone maintaining a second list.
        let exported = bindings();
        let inspected =
            inspect_context_attribution(&exported).expect("the real grammar carries no actor");
        assert_eq!(
            inspected,
            CONTEXT_CLIENT_SUBMITTABLE.len(),
            "the guard did not inspect every client-submittable type"
        );

        // Seeded: an actor field on a client-submittable type must be refused.
        let needle = "export type RecordContextFact_Serialize = {";
        assert_eq!(
            exported.matches(needle).count(),
            1,
            "mutation target drifted"
        );
        let mutated = exported.replacen(needle, &format!("{needle} actor: string;"), 1);
        let error = inspect_context_attribution(&mutated)
            .expect_err("an actor on a client-submittable type must be refused");
        assert!(error.contains("actor"), "{error}");
        assert!(error.contains("CTX-12"), "{error}");

        // And a missing subject is a broken guard, never a clean grammar.
        let error = inspect_context_attribution("export type Unrelated = number;")
            .expect_err("a missing subject must not read as clean");
        assert!(error.contains("inspected 0"), "{error}");
    }

    #[test]
    fn the_attribution_guard_does_not_fire_on_a_name_that_merely_contains_the_word() {
        // `author` is in the forbidden set and `authority_digest` contains it.
        // The Rust-side version of this check compared substrings and tripped on
        // exactly that; the tempting repair is to drop `author` from the set,
        // which deletes the check for one of the names a spoofed record is most
        // likely to use. Property names are compared whole instead.
        let sample = "export type RecordContextFact_Serialize = { authority_digest: string }\n\
                      export type RecordContextFact_Deserialize = { authority_digest: string }\n\
                      export type ContextFact_Serialize = { authored_at: string }\n\
                      export type ContextFact_Deserialize = { user_facing_note: string }\n";
        assert_eq!(
            inspect_context_attribution(sample).expect("neighbours are not matches"),
            CONTEXT_CLIENT_SUBMITTABLE.len()
        );
    }

    #[test]
    fn context_root_guard_rejects_a_removed_registration_after_counting() {
        let source = include_str!("contract.rs");
        assert_eq!(
            inspect_context_truth_registry(source).expect("real registry is complete"),
            CONTEXT_TRUTH_ROOTS.len()
        );
        let target = format!(".register::<{}>()", "ReleaseSupplement");
        assert_eq!(
            source.matches(&target).count(),
            1,
            "mutation target drifted"
        );
        let mutated = source.replacen(&target, ".register::<RemovedReleaseSupplement>()", 1);
        let error = inspect_context_truth_registry(&mutated)
            .expect_err("a removed Context root must fail the registry guard");
        assert!(
            error.contains("inspected 3 registered context truth roots"),
            "{error}"
        );
    }

    #[test]
    fn context_schema_guard_rejects_a_removed_cardinality_constraint_after_counting() {
        let schema = include_str!("../../schema/0001_contract.sql");
        inspect_context_truth_schema(schema).expect("real Context schema is complete");
        let target = "CONSTRAINT context_release_one_per_manifest UNIQUE (manifest_id)";
        assert_eq!(schema.matches(target).count(), 1, "mutation target drifted");
        let mutated = schema.replacen(target, "", 1);
        let error = inspect_context_truth_schema(&mutated)
            .expect_err("a removed uniqueness constraint must fail the schema guard");
        assert!(
            error.contains("inspected 4 context table declarations, 3 cardinality constraints"),
            "{error}"
        );
    }

    #[test]
    fn context_schema_guard_rejects_a_removed_value_validator_after_counting() {
        let schema = include_str!("../../schema/0001_contract.sql");
        inspect_context_truth_schema(schema).expect("real Context schema is complete");
        let target = "CREATE FUNCTION gwk.context_evidence_ids_are_valid(";
        assert_eq!(schema.matches(target).count(), 1, "mutation target drifted");
        let mutated = schema.replacen(
            target,
            "CREATE FUNCTION gwk.removed_evidence_ids_validator(",
            1,
        );
        let error = inspect_context_truth_schema(&mutated)
            .expect_err("a removed value validator must fail the schema guard");
        assert!(error.contains("1 value-validator functions"), "{error}");
    }

    #[test]
    fn context_schema_guard_rejects_a_removed_evidence_check_after_counting() {
        let schema = include_str!("../../schema/0001_contract.sql");
        inspect_context_truth_schema(schema).expect("real Context schema is complete");
        assert_eq!(
            schema.matches(CONTEXT_EVIDENCE_GUARD).count(),
            CONTEXT_TRUTH_TABLES.len(),
            "mutation target drifted"
        );
        let mutated = schema.replacen(CONTEXT_EVIDENCE_GUARD, "CHECK (true)", 1);
        let error = inspect_context_truth_schema(&mutated)
            .expect_err("a removed evidence check must fail the schema guard");
        assert!(error.contains("4 value-validation constraints"), "{error}");
    }

    #[test]
    fn context_schema_guard_rejects_a_removed_participation_check_after_counting() {
        let schema = include_str!("../../schema/0001_contract.sql");
        inspect_context_truth_schema(schema).expect("real Context schema is complete");
        assert_eq!(
            schema.matches(CONTEXT_PARTICIPATION_GUARD).count(),
            1,
            "mutation target drifted"
        );
        let mutated = schema.replacen(CONTEXT_PARTICIPATION_GUARD, "CHECK (true)", 1);
        let error = inspect_context_truth_schema(&mutated)
            .expect_err("a removed participation check must fail the schema guard");
        assert!(error.contains("4 value-validation constraints"), "{error}");
    }

    #[test]
    fn check_orphans_flags_an_unexpected_json_file() {
        // Private, unpredictable, auto-cleaned dir — never a guessable name in
        // a world-writable /tmp that a symlink race could redirect.
        let tmp = tempfile::tempdir().expect("create temp dir");
        let dir = tmp.path();
        std::fs::write(dir.join("task.json"), "{}").expect("write expected golden");
        std::fs::write(dir.join("renamed-task.json"), "{}").expect("write orphan golden");

        let mut drift = Vec::new();
        check_orphans(dir, &["task.json"], &mut drift);

        // `tmp` drops at end of scope and removes the dir.
        assert_eq!(
            drift.len(),
            1,
            "expected exactly one orphan finding: {drift:?}"
        );
        assert!(drift[0].contains("renamed-task.json"));
    }
}
