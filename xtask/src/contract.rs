//! `cargo run -p xtask -- contract [--check]` — the contract codegen gate.
//!
//! Generates, deterministically (no timestamps, no local paths):
//!   * `contracts/bindings.ts` — the TypeScript contract from gwk-domain + gwk-theme
//!   * `contracts/signal-theme.json` — the SIGNAL tokens as data
//!   * `contracts/goldens/*.json` — Rust-serialized fixtures the bun tests decode
//!
//! `--check` regenerates in memory and fails on ANY drift from the committed
//! artifacts, then round-trips `contracts/goldens-ts/*.json` (re-emitted by the
//! bun tests, committed) back through serde to prove TS -> Rust decoding agrees.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use gwk_domain::blob::{BlobAddress, BlobDescriptor};
use gwk_domain::checkpoint::{CHECKPOINT_SCHEMA_VERSION, Checkpoint};
use gwk_domain::command::KernelCommand;
use gwk_domain::entity::{Attempt, Budget, Command, Message, Task};
use gwk_domain::envelope::{Actor, CommandEnvelope, EventEnvelope, Origin, PayloadRef};
use gwk_domain::fsm::{AttemptState, CommandState, MessageState, Outcome, TaskState};
use gwk_domain::ids::{
    AggregateId, AttemptId, BlobUploadId, ByteCount, CommandId, CorrelationId, CostMicros,
    EngineId, EventCount, EventId, IdempotencyKey, LeaseId, MessageId, ProjectId, RequestId, Seq,
    TaskId, Timestamp,
};
use gwk_domain::inherited::{BudgetCursor, OrchestratorCheckpoint, PendingApproval};
use gwk_domain::protocol::{
    CapabilityName, ClientControl, KernelErrorCode, KernelRequest, KernelResult, PROTOCOL_MINOR,
    ProjectionKind, ProtocolVersion, ServerControl,
};
use gwk_domain::transition::TransitionResult;

const HEADER: &str = "\
// GridWork contract bindings.
// Generated from gwk-domain + gwk-theme by `cargo run -p xtask -- contract`.
// DO NOT EDIT — regenerate instead; CI diffs this file against the source.
";

fn repo_root() -> PathBuf {
    // xtask always runs from the workspace via cargo; CARGO_MANIFEST_DIR is xtask/.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask has a parent dir")
        .to_path_buf()
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
        .register::<gwk_theme::Token>();
    // PhasesFormat, not the unified Format: `skip_serializing_if` (the
    // tri-state omission) is direction-dependent, which unified mode refuses
    // to represent.
    specta_typescript::Typescript::default()
        .header(HEADER)
        .export(&types, specta_serde::PhasesFormat)
        .expect("typescript export")
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
        result_schema_ref: None,
        provider_session_ref: Some("sess-9".into()),
        runtime_ref: None,
        runtime_started_at: Some(Timestamp::new("2026-07-27T11:00:00Z")),
        exit_code: None,
        provider_terminal_event: None,
        result_valid: None,
        evidence_manifest_ref: None,
        gate_result: None,
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

fn golden_client_control() -> Vec<ClientControl> {
    vec![
        ClientControl::Hello {
            protocol_major: ProtocolVersion::V1,
            protocol_minor: PROTOCOL_MINOR,
            capabilities: vec![capability("event_subscribe"), capability("blob")],
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
    ]
}

fn golden_server_control() -> Vec<ServerControl> {
    vec![
        ServerControl::HelloAck {
            protocol_major: ProtocolVersion::V1,
            protocol_minor: PROTOCOL_MINOR,
            // The INTERSECTION: the client asked for blob too, this kernel
            // grants only the subscription.
            capabilities: vec![capability("event_subscribe")],
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
        ("command-envelope.json", pretty(&golden_command_envelope())),
        ("task.json", pretty(&golden_task())),
        ("attempt.json", pretty(&golden_attempt())),
        ("message.json", pretty(&golden_message())),
        (
            "command-verification-complete.json",
            pretty(&golden_command_terminal()),
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
    const REGISTERED_ROOT_COUNT: usize = 24;

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
        let source = include_str!("contract.rs");
        let sig_at = source
            .find("fn bindings() -> String {")
            .expect("bindings() signature present");
        let close_rel = source[sig_at..]
            .find("\n}\n")
            .expect("bindings() closing brace");
        let body = &source[sig_at..sig_at + close_rel];
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
