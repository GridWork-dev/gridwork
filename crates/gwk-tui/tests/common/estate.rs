//! The seeded workday estate: one coherent fictional GridWork Agent OS day
//! across four districts, rich enough that density and legibility judgments
//! made against it are real. Every lens's scenario function reads a facet of
//! the SAME day rather than an isolated fixture, so the goldens read as one
//! estate viewed five ways.
//!
//! `#![allow(dead_code)]` for the same reason as `common/mod.rs`: not every
//! test binary that pulls in this module exercises every scenario fn.
#![allow(dead_code)]

use gwk_domain::entity::{
    Attempt, AttentionItem, Budget, CostEntry, DispatchNode, EngineSession, Gate, IngestedRecord,
    Lease, Message, PtySession, Receipt, Task, WorkflowRun, Worktree,
};
use gwk_domain::envelope::{Actor, EventEnvelope, Origin};
use gwk_domain::frame::{CellColor, CellStyle, PtyAnsiSlot, PtyFrame, StyledCell};
use gwk_domain::fsm::{AttemptState, GateVerdict, LeaseMode, LeaseState, MessageState, TaskState};
use gwk_domain::ids::{
    AggregateId, AttemptId, AttentionItemId, CorrelationId, CostEntryId, CostMicros,
    DispatchNodeId, EngineId, EngineSessionId, EventId, GateId, IngestedRecordId, LeaseId,
    MessageId, ProjectId, PtyFrameSeq, PtySessionGeneration, PtySessionId, ReceiptId, RequestId,
    Seq, TaskId, Timestamp, TokenCount, WorkflowRunId, WorktreeId,
};
use gwk_domain::ingestion::IngestionKind;
use gwk_domain::protocol::{KernelResult, ServerControl};
use gwk_tui::board::{BoardState, BoardView, EventTail};
use gwk_tui::config::{ConfigFileState, ConfigPath, ConfigState, EditRoute};
use gwk_tui::drilldown::DrilldownState;
use gwk_tui::hall::{
    Agent, AgentId, AgentState, Attention, AttentionId as HallAttentionId, District, DistrictId,
    Focus, FrameInput, Station, StationId,
};
use gwk_tui::queue::QueueState;
use gwk_tui::replay::ReplayTimeline;

/// The projector watermark every populated scenario reports as-of.
const WATERMARK: u64 = 221;
/// The clock [`estate_queue_state`] assembles its frame against.
const NOW: &str = "2026-08-11T17:30:00Z";

fn ts(value: &str) -> Timestamp {
    Timestamp::new(value)
}

// ---------------------------------------------------------------------------
// Hall — the estate frame
// ---------------------------------------------------------------------------

fn district_id(value: &str) -> DistrictId {
    DistrictId::new(value).expect("district id")
}

fn station_id(value: &str) -> StationId {
    StationId::new(value).expect("station id")
}

fn agent_id(value: &str) -> AgentId {
    AgentId::new(value).expect("agent id")
}

fn hall_attention_id(value: &str) -> HallAttentionId {
    HallAttentionId::new(value).expect("attention id")
}

fn agent(id: &str, role: Option<&str>, state: AgentState, seq: u64) -> Agent {
    Agent {
        id: agent_id(id),
        role: role.map(str::to_owned),
        state,
        duration: None,
        changed_seq: Seq::new(seq),
    }
}

fn station(id: &str, ordinal: u16, label: &str, agents: Vec<Agent>, seq: u64) -> Station {
    Station {
        id: station_id(id),
        label: label.to_owned(),
        template_ordinal: ordinal,
        agents,
        changed_seq: Seq::new(seq),
    }
}

fn district(id: &str, label: &str, stations: Vec<Station>, aged_done: usize, seq: u64) -> District {
    District {
        id: district_id(id),
        label: label.to_owned(),
        stations,
        aged_done,
        changed_seq: Seq::new(seq),
    }
}

/// Four districts, fifteen agents covering all eleven [`AgentState`]
/// variants, and a role mix spanning the six glyph-whitelisted bare names
/// (`orchestrator`/`researcher`/`architect`/`implementer`/`reviewer`/
/// `auditor`), five free-form GridWork role strings, and two agents with no
/// role at all. Four of the free-form roles carry the `gw-` prefix and now
/// resolve past it — `gw-security-auditor` to the `auditor` family mark, the
/// rest to a letter taken after the prefix rather than from it. Only
/// `general-purpose`, which names no family, still falls back to its own
/// first letter; the fixture keeps it so the escape stays covered.
pub fn estate_frame_input() -> FrameInput {
    FrameInput {
        districts: vec![
            district(
                "district-kernel",
                "Kernel",
                vec![
                    station(
                        "station-pty",
                        1,
                        "pty",
                        vec![
                            agent(
                                "agent-pty-impl",
                                Some("implementer"),
                                AgentState::Running,
                                101,
                            ),
                            agent(
                                "agent-pty-review",
                                Some("reviewer"),
                                AgentState::NeedsAttention,
                                102,
                            ),
                        ],
                        102,
                    ),
                    station(
                        "station-blob",
                        2,
                        "blob",
                        vec![
                            agent(
                                "agent-blob-arch",
                                Some("architect"),
                                AgentState::Blocked,
                                103,
                            ),
                            agent(
                                "agent-blob-idle",
                                Some("gw-rust-pro"),
                                AgentState::Idle,
                                104,
                            ),
                        ],
                        104,
                    ),
                ],
                0,
                104,
            ),
            district(
                "district-tui",
                "TUI",
                vec![
                    station(
                        "station-lens",
                        1,
                        "lens",
                        vec![
                            agent(
                                "agent-tui-impl",
                                Some("implementer"),
                                AgentState::Running,
                                105,
                            ),
                            agent(
                                "agent-tui-audit",
                                Some("auditor"),
                                AgentState::Starting,
                                106,
                            ),
                        ],
                        106,
                    ),
                    station(
                        "station-harness",
                        2,
                        "harness",
                        vec![
                            agent(
                                "agent-tui-general",
                                Some("general-purpose"),
                                AgentState::Queued,
                                107,
                            ),
                            agent(
                                "agent-tui-cancel",
                                Some("reviewer"),
                                AgentState::Canceling,
                                108,
                            ),
                        ],
                        108,
                    ),
                ],
                0,
                108,
            ),
            district(
                "district-site",
                "Site",
                vec![
                    station(
                        "station-docs",
                        1,
                        "docs",
                        vec![
                            agent(
                                "agent-docs-writer",
                                Some("gw-devrel-writer"),
                                AgentState::Done,
                                109,
                            ),
                            agent("agent-docs-none", None, AgentState::Unknown, 110),
                        ],
                        110,
                    ),
                    station(
                        "station-deploy",
                        2,
                        "deploy",
                        vec![agent(
                            "agent-deploy-arch",
                            Some("architect"),
                            AgentState::Failed,
                            111,
                        )],
                        111,
                    ),
                ],
                2,
                111,
            ),
            district(
                "district-ops",
                "Ops",
                vec![
                    station(
                        "station-ci",
                        1,
                        "ci",
                        vec![
                            agent(
                                "agent-ci-test",
                                Some("gw-test-automator"),
                                AgentState::Canceled,
                                112,
                            ),
                            agent("agent-ci-watch", Some("researcher"), AgentState::Idle, 113),
                        ],
                        113,
                    ),
                    station(
                        "station-audit",
                        2,
                        "audit",
                        vec![
                            agent(
                                "agent-audit-sec",
                                Some("gw-security-auditor"),
                                AgentState::NeedsAttention,
                                114,
                            ),
                            agent("agent-audit-done", Some("auditor"), AgentState::Done, 115),
                        ],
                        115,
                    ),
                ],
                3,
                115,
            ),
        ],
        focus: Some(Focus {
            district: district_id("district-tui"),
            changed_seq: Seq::new(107),
        }),
        attention: vec![
            Attention {
                id: hall_attention_id("attention-pty"),
                district: district_id("district-kernel"),
                unresolved: true,
                changed_seq: Seq::new(102),
            },
            Attention {
                id: hall_attention_id("attention-ci"),
                district: district_id("district-ops"),
                unresolved: true,
                changed_seq: Seq::new(114),
            },
            Attention {
                id: hall_attention_id("attention-blob"),
                district: district_id("district-kernel"),
                unresolved: false,
                changed_seq: Seq::new(103),
            },
        ],
        watermark: Some(Seq::new(WATERMARK)),
    }
}

pub fn empty_frame_input() -> FrameInput {
    FrameInput {
        districts: Vec::new(),
        focus: None,
        attention: Vec::new(),
        watermark: None,
    }
}

// ---------------------------------------------------------------------------
// Raw domain facts — the source every Board/Queue/Config scenario draws from
// ---------------------------------------------------------------------------

fn task(
    id: &str,
    title: &str,
    state: TaskState,
    priority: i32,
    created: &str,
    updated: &str,
) -> Task {
    Task {
        id: TaskId::new(id),
        version: 1,
        state,
        kind: Some("phase".into()),
        title: Some(title.into()),
        spec_ref: None,
        project: Some("gridwork".into()),
        priority: Some(priority),
        tracker_ref: None,
        created_at: ts(created),
        updated_at: ts(updated),
    }
}

fn attempt(
    id: &str,
    task: &str,
    engine: &str,
    role: Option<&str>,
    state: AttemptState,
    created: &str,
    updated: &str,
) -> Attempt {
    Attempt {
        id: AttemptId::new(id),
        version: 1,
        state,
        task_id: TaskId::new(task),
        engine: EngineId::new(engine),
        capability: Some("code_write".into()),
        role: role.map(str::to_owned),
        model_lane: Some("standard".into()),
        permission_profile: None,
        worktree_lease_id: None,
        base_sha: None,
        budget: None,
        provider_session_ref: None,
        runtime_ref: None,
        runtime_started_at: None,
        exit_code: None,
        provider_terminal_event: None,
        result_valid: None,
        evidence_manifest_ref: None,
        created_at: ts(created),
        updated_at: ts(updated),
    }
}

fn dispatch_node(
    id: &str,
    attempt: &str,
    parent: Option<&str>,
    label: &str,
    state: &str,
) -> DispatchNode {
    DispatchNode {
        id: DispatchNodeId::new(id),
        version: 1,
        parent_id: parent.map(DispatchNodeId::new),
        attempt_id: Some(AttemptId::new(attempt)),
        kind: "subagent".into(),
        state: state.into(),
        label: Some(label.into()),
        created_at: ts("2026-08-11T09:45:00Z"),
        updated_at: ts("2026-08-11T09:50:00Z"),
    }
}

fn message(
    id: &str,
    sender: &str,
    recipient: &str,
    kind: &str,
    state: MessageState,
    created: &str,
    updated: &str,
) -> Message {
    Message {
        id: MessageId::new(id),
        version: 1,
        state,
        idempotency_key: gwk_domain::ids::IdempotencyKey::new(format!("k-{id}")),
        correlation_id: None,
        reply_to: None,
        sender: Some(sender.into()),
        recipient: Some(recipient.into()),
        channel: Some("dispatch".into()),
        kind: Some(kind.into()),
        payload: None,
        deadline: None,
        delivery_attempts: 1,
        dead_letter_reason: None,
        delivery_refs: None,
        created_at: ts(created),
        updated_at: ts(updated),
    }
}

fn gate(
    id: &str,
    question: &str,
    verdict: GateVerdict,
    chosen: Option<&str>,
    kind: &str,
    created: &str,
    updated: &str,
) -> Gate {
    Gate {
        id: GateId::new(id),
        version: 1,
        attempt_id: None,
        phase_ref: Some("7p-tui-snapshot-harness".into()),
        kind: Some(kind.into()),
        question: Some(question.into()),
        options: Some(vec!["allow".into(), "deny".into()]),
        verdict,
        chosen_option: chosen.map(str::to_owned),
        evidence_ref: None,
        created_at: ts(created),
        updated_at: ts(updated),
    }
}

fn attention_item(
    id: &str,
    kind: &str,
    summary: &str,
    subject_ref: &str,
    priority: Option<i32>,
    raised: &str,
) -> AttentionItem {
    AttentionItem {
        id: AttentionItemId::new(id),
        kind: kind.into(),
        summary: summary.into(),
        subject_ref: Some(subject_ref.into()),
        raised_by: None,
        priority,
        raised_at: ts(raised),
        acked_at: None,
        muted_until: None,
        resolved_at: None,
        resolution: None,
    }
}

fn receipt(
    id: &str,
    action: &str,
    subject: (&str, &str),
    edge: (Option<&str>, Option<&str>),
    basis: Option<&str>,
    at: &str,
) -> Receipt {
    Receipt {
        id: ReceiptId::new(id),
        actor: Actor {
            kind: "kernel".into(),
            id: None,
        },
        action: action.into(),
        subject_type: subject.0.into(),
        subject_id: subject.1.into(),
        from: edge.0.map(str::to_owned),
        to: edge.1.map(str::to_owned),
        observed_basis: basis.map(str::to_owned),
        ts: ts(at),
    }
}

/// The core cost fields every entry sets; `engine_session_id`/
/// `dispatch_node_id`/`cost_micros`/`cost_is_estimate` stay the caller's to
/// overwrite after — keeping this constructor at clippy's seven-argument
/// ceiling.
fn cost(
    id: &str,
    attempt: Option<&str>,
    engine: &str,
    model: &str,
    input: u64,
    output: u64,
    recorded: &str,
) -> CostEntry {
    CostEntry {
        id: CostEntryId::new(id),
        attempt_id: attempt.map(AttemptId::new),
        engine_session_id: None,
        dispatch_node_id: None,
        engine: EngineId::new(engine),
        model: Some(model.into()),
        input_tokens: Some(TokenCount::new(input)),
        cached_input_tokens: None,
        cache_write_tokens: None,
        output_tokens: Some(TokenCount::new(output)),
        reasoning_tokens: None,
        cost_micros: None,
        cost_is_estimate: None,
        recorded_at: ts(recorded),
    }
}

fn engine_session(id: &str, attempt: &str, engine: &str, ended: Option<&str>) -> EngineSession {
    EngineSession {
        id: EngineSessionId::new(id),
        attempt_id: AttemptId::new(attempt),
        engine: EngineId::new(engine),
        provider_session_ref: Some(format!("prov-{id}")),
        started_at: ts("2026-08-11T09:40:00Z"),
        ended_at: ended.map(ts),
    }
}

fn worktree(
    id: &str,
    branch: &str,
    dirty: bool,
    unpushed: bool,
    released: Option<&str>,
) -> Worktree {
    Worktree {
        id: WorktreeId::new(id),
        repo: "gridwork".into(),
        path: format!("worktrees/{}", id.trim_start_matches("wt-")),
        branch: branch.into(),
        base_sha: Some("e4eca2f0000000000000000000000000000000".into()),
        lease_id: Some(LeaseId::new(id.replacen("wt-", "ls-", 1))),
        dirty,
        unpushed,
        released_at: released.map(ts),
        disposition: released.map(|_| "released".into()),
        created_at: ts("2026-08-11T09:00:00Z"),
    }
}

fn lease(
    id: &str,
    state: LeaseState,
    holder: &str,
    dirty: bool,
    unpushed: bool,
    expires: Option<&str>,
) -> Lease {
    Lease {
        id: LeaseId::new(id),
        version: 1,
        state,
        mode: LeaseMode::Exclusive,
        holder: Some(holder.into()),
        scope: Some(format!("worktree:{}", id.trim_start_matches("ls-"))),
        repo: Some("gridwork".into()),
        path: Some(format!("worktrees/{}", id.trim_start_matches("ls-"))),
        branch: None,
        base_sha: None,
        fence_token: None,
        heartbeat_at: Some(ts("2026-08-11T17:00:00Z")),
        expires_at: expires.map(ts),
        dirty,
        unpushed,
        disposition: None,
        created_at: ts("2026-08-11T09:00:00Z"),
        updated_at: ts("2026-08-11T17:00:00Z"),
    }
}

fn ingested(
    id: &str,
    kind: IngestionKind,
    payload: serde_json::Value,
    seq: u64,
    at: &str,
) -> IngestedRecord {
    IngestedRecord {
        id: IngestedRecordId::new(id),
        kind,
        payload,
        payload_ref: None,
        ingested_by: None,
        event_seq: Seq::new(seq),
        ingested_at: ts(at),
    }
}

/// `step`/`closed_at` stay the caller's to overwrite after — keeping this
/// constructor at clippy's seven-argument ceiling.
fn workflow_run(
    id: &str,
    state: &str,
    task: &str,
    title: &str,
    opened: &str,
    updated: &str,
) -> WorkflowRun {
    WorkflowRun {
        id: WorkflowRunId::new(id),
        version: 3,
        state: state.into(),
        step: None,
        template_ref: "seven-act@1".into(),
        template_sha256: None,
        task_id: Some(TaskId::new(task)),
        title: Some(title.into()),
        opened_at: ts(opened),
        updated_at: ts(updated),
        closed_at: None,
    }
}

fn pty_session(
    id: &str,
    state: &str,
    generation: &str,
    title: &str,
    opened: &str,
    updated: &str,
) -> PtySession {
    PtySession {
        id: PtySessionId::new(id),
        version: 4,
        state: state.into(),
        generation: PtySessionGeneration::new(generation),
        attach_count: 0,
        detach_count: 0,
        title: Some(title.into()),
        opened_at: ts(opened),
        updated_at: ts(updated),
        closed_at: None,
    }
}

/// The two hosted PTY sessions of the day: the kernel worktree shell still
/// running (its `pty-1`/`gen-3` are the same session [`drilldown_attached`]
/// paints, kept consistent across lenses), and the release-notes editor
/// already closed. No lens's state struct carries a `pty_session` list today
/// — Drilldown attaches to a live one over wire messages, not this entity —
/// so this pair currently has no renderer; kept seeded per the brief's
/// minimum bar for whenever a Fleet/estate panel projects `pty_session` rows.
pub fn estate_pty_sessions() -> (PtySession, PtySession) {
    let mut running = pty_session(
        "pty-1",
        "running",
        "gen-3",
        "kernel worktree shell",
        "2026-08-11T10:21:00Z",
        "2026-08-11T17:28:00Z",
    );
    running.attach_count = 2;
    running.detach_count = 1;

    let mut closed = pty_session(
        "pty-2",
        "closed",
        "gen-1",
        "release notes editor",
        "2026-08-11T09:16:00Z",
        "2026-08-11T09:45:00Z",
    );
    closed.attach_count = 1;
    closed.detach_count = 1;
    closed.closed_at = Some(ts("2026-08-11T09:45:00Z"));

    (running, closed)
}

fn event(id: &str, seq: u64, aggregate: &str, kind: &str, at: &str) -> EventEnvelope {
    EventEnvelope {
        event_id: EventId::new(id),
        project_id: ProjectId::new("system"),
        aggregate_type: aggregate.into(),
        aggregate_id: AggregateId::new(format!("{aggregate}-{seq}")),
        aggregate_version: 1,
        event_type: kind.into(),
        schema_version: 1,
        global_sequence: Seq::new(seq),
        occurred_at: ts(at),
        appended_at: ts(at),
        actor: Actor {
            kind: "kernel".into(),
            id: None,
        },
        origin: Origin {
            system: "gwk".into(),
            r#ref: None,
        },
        causation_id: None,
        correlation_id: None,
        idempotency_key: None,
        payload: serde_json::json!({ "seq": seq }),
        payload_ref: None,
    }
}

/// Every raw fact the workday holds, before a lens narrows it to its own
/// shape (Board reads all of it; Queue reads attention/gates/receipts/
/// messages only — gates never reach the Board, by contract).
struct Raw {
    tasks: Vec<Task>,
    attempts: Vec<Attempt>,
    nodes: Vec<DispatchNode>,
    messages: Vec<Message>,
    gates: Vec<Gate>,
    attention: Vec<AttentionItem>,
    receipts: Vec<Receipt>,
    costs: Vec<CostEntry>,
    sessions: Vec<EngineSession>,
    worktrees: Vec<Worktree>,
    leases: Vec<Lease>,
    ingested: Vec<IngestedRecord>,
    runs: Vec<WorkflowRun>,
    events: Vec<EventEnvelope>,
}

fn raw() -> Raw {
    Raw {
        tasks: vec![
            task(
                "t-pty-lifecycle",
                "cut the pty_session lifecycle over to receipts",
                TaskState::Working,
                1,
                "2026-08-11T08:00:00Z",
                "2026-08-11T11:41:00Z",
            ),
            task(
                "t-tui-harness",
                "build the seeded-estate snapshot harness",
                TaskState::Working,
                2,
                "2026-08-11T09:00:00Z",
                "2026-08-11T17:20:00Z",
            ),
            task(
                "t-release-notes",
                "write the 0.0.3 release notes",
                TaskState::Submitted,
                3,
                "2026-08-11T09:05:00Z",
                "2026-08-11T09:05:00Z",
            ),
            task(
                "t-ship-0-3",
                "ship 0.0.3",
                TaskState::Completed,
                1,
                "2026-08-11T08:00:00Z",
                "2026-08-11T09:14:00Z",
            ),
            task(
                "t-flaky-ci",
                "stabilize the flaky CI leg",
                TaskState::InputRequired,
                2,
                "2026-08-11T09:30:00Z",
                "2026-08-11T12:00:00Z",
            ),
            task(
                "t-audit-log",
                "backfill the audit receipt ledger",
                TaskState::Failed,
                1,
                "2026-08-11T09:45:00Z",
                "2026-08-11T16:20:00Z",
            ),
        ],
        attempts: {
            let mut ship = attempt(
                "at-ship-done",
                "t-ship-0-3",
                "codex",
                Some("implementer"),
                AttemptState::Succeeded,
                "2026-08-11T08:45:00Z",
                "2026-08-11T09:10:00Z",
            );
            ship.exit_code = Some(0);
            ship.provider_session_ref = Some("prov-ship-1".into());

            let mut pty_impl = attempt(
                "at-pty-impl",
                "t-pty-lifecycle",
                "codex",
                Some("implementer"),
                AttemptState::Running,
                "2026-08-11T09:40:00Z",
                "2026-08-11T11:41:00Z",
            );
            pty_impl.worktree_lease_id = Some(LeaseId::new("ls-pty"));
            pty_impl.base_sha = Some("e4eca2f0000000000000000000000000000000".into());
            pty_impl.budget = Some(Budget {
                max_tokens: Some(2_000_000),
                max_tool_calls: Some(150),
                max_wall_ms: Some(2_400_000),
                max_cost_micros: Some(CostMicros::new(5_000_000)),
            });

            let pty_review = attempt(
                "at-pty-review",
                "t-pty-lifecycle",
                "claude",
                Some("reviewer"),
                AttemptState::Blocked,
                "2026-08-11T09:48:00Z",
                "2026-08-11T10:30:00Z",
            );

            let pty_start = attempt(
                "at-pty-start",
                "t-pty-lifecycle",
                "claude",
                Some("researcher"),
                AttemptState::Starting,
                "2026-08-11T10:35:00Z",
                "2026-08-11T10:36:00Z",
            );

            let mut tui_impl = attempt(
                "at-tui-impl",
                "t-tui-harness",
                "claude",
                Some("implementer"),
                AttemptState::Running,
                "2026-08-11T10:00:00Z",
                "2026-08-11T17:20:00Z",
            );
            tui_impl.worktree_lease_id = Some(LeaseId::new("ls-tui"));
            tui_impl.budget = Some(Budget {
                max_tokens: Some(1_500_000),
                max_tool_calls: None,
                max_wall_ms: None,
                max_cost_micros: None,
            });

            let tui_queue = attempt(
                "at-tui-queue",
                "t-tui-harness",
                "codex",
                Some("gw-rust-pro"),
                AttemptState::Queued,
                "2026-08-11T10:07:00Z",
                "2026-08-11T10:07:00Z",
            );

            let release_lease = attempt(
                "at-release-lease",
                "t-release-notes",
                "codex",
                Some("gw-devrel-writer"),
                AttemptState::Leased,
                "2026-08-11T09:15:00Z",
                "2026-08-11T09:16:00Z",
            );

            let flaky_cancel = attempt(
                "at-flaky-cancel",
                "t-flaky-ci",
                "claude",
                Some("gw-test-automator"),
                AttemptState::Canceling,
                "2026-08-11T12:00:00Z",
                "2026-08-11T12:02:00Z",
            );

            let flaky_canceled = attempt(
                "at-flaky-canceled",
                "t-flaky-ci",
                "claude",
                None,
                AttemptState::Canceled,
                "2026-08-11T11:00:00Z",
                "2026-08-11T11:05:00Z",
            );

            let mut audit_fail = attempt(
                "at-audit-fail",
                "t-audit-log",
                "claude",
                Some("auditor"),
                AttemptState::Failed,
                "2026-08-11T12:05:00Z",
                "2026-08-11T15:10:00Z",
            );
            audit_fail.exit_code = Some(1);

            let mut audit_unknown = attempt(
                "at-audit-unknown",
                "t-audit-log",
                "codex",
                None,
                AttemptState::Unknown,
                "2026-08-11T12:10:00Z",
                "2026-08-11T16:20:00Z",
            );
            audit_unknown.provider_terminal_event = Some("connection_reset".into());

            vec![
                ship,
                pty_impl,
                pty_review,
                pty_start,
                tui_impl,
                tui_queue,
                release_lease,
                flaky_cancel,
                flaky_canceled,
                audit_fail,
                audit_unknown,
            ]
        },
        nodes: vec![
            dispatch_node("d-pty-root", "at-pty-impl", None, "implementer", "running"),
            dispatch_node(
                "d-pty-recon",
                "at-pty-impl",
                Some("d-pty-root"),
                "recon",
                "completed",
            ),
            dispatch_node(
                "d-pty-lint",
                "at-pty-impl",
                Some("d-pty-recon"),
                "lint",
                "registered",
            ),
            dispatch_node("d-tui-root", "at-tui-impl", None, "implementer", "running"),
            dispatch_node(
                "d-tui-review",
                "at-tui-impl",
                Some("d-tui-root"),
                "gw-code-reviewer",
                "registered",
            ),
        ],
        messages: {
            let mut brief = message(
                "m-brief",
                "orchestrator",
                "researcher",
                "brief",
                MessageState::Delivered,
                "2026-08-11T09:30:00Z",
                "2026-08-11T09:31:00Z",
            );
            brief.correlation_id = Some(CorrelationId::new("c-pty-1"));

            let mut findings = message(
                "m-findings",
                "researcher",
                "orchestrator",
                "findings",
                MessageState::Applied,
                "2026-08-11T09:55:00Z",
                "2026-08-11T09:58:00Z",
            );
            findings.correlation_id = Some(CorrelationId::new("c-pty-1"));
            findings.reply_to = Some(MessageId::new("m-brief"));

            let mut alert = message(
                "m-alert",
                "watchdog",
                "orchestrator",
                "alert",
                MessageState::DeadLetter,
                "2026-08-11T16:40:00Z",
                "2026-08-11T16:45:00Z",
            );
            alert.dead_letter_reason = Some("nobody listening".into());
            alert.delivery_attempts = 3;

            let review_req = message(
                "m-review-req",
                "orchestrator",
                "reviewer",
                "review-request",
                MessageState::Accepted,
                "2026-08-11T10:20:00Z",
                "2026-08-11T10:20:00Z",
            );

            let mut ack = message(
                "m-ack",
                "reviewer",
                "orchestrator",
                "ack",
                MessageState::Acknowledged,
                "2026-08-11T10:24:00Z",
                "2026-08-11T10:24:00Z",
            );
            ack.reply_to = Some(MessageId::new("m-review-req"));

            let status = message(
                "m-status",
                "implementer",
                "orchestrator",
                "status",
                MessageState::Rejected,
                "2026-08-11T17:05:00Z",
                "2026-08-11T17:06:00Z",
            );

            vec![brief, findings, alert, review_req, ack, status]
        },
        gates: vec![
            gate(
                "g-deploy",
                "restart the kernel service after the pty_session receipt fix?",
                GateVerdict::Pending,
                None,
                "deploy",
                "2026-08-11T17:00:00Z",
                "2026-08-11T17:00:00Z",
            ),
            gate(
                "g-lint",
                "run cargo clippy --all-targets before merge?",
                GateVerdict::Pass,
                Some("allow"),
                "gate",
                "2026-08-11T10:20:00Z",
                "2026-08-11T10:23:00Z",
            ),
            gate(
                "g-migrate",
                "apply the pty_session lifecycle migration to prod?",
                GateVerdict::Fail,
                Some("deny"),
                "data_migration",
                "2026-08-11T16:00:00Z",
                "2026-08-11T16:05:00Z",
            ),
            gate(
                "g-canary",
                "promote the canary build to 100%?",
                GateVerdict::Partial,
                Some("hold"),
                "deploy",
                "2026-08-11T17:10:00Z",
                "2026-08-11T17:15:00Z",
            ),
        ],
        attention: {
            let mut kek = attention_item(
                "att-kek",
                "operator",
                "KEK rotation window closes in 2 hours",
                "worktree:wt-pty",
                Some(0),
                "2026-08-11T10:19:00Z",
            );
            kek.raised_by = Some(Actor {
                kind: "kernel".into(),
                id: None,
            });

            let flaky = attention_item(
                "att-flaky",
                "watchdog",
                "t-flaky-ci attempt is stuck canceling",
                "attempt:at-flaky-cancel",
                Some(2),
                "2026-08-11T10:20:00Z",
            );

            let mut disk = attention_item(
                "att-disk",
                "watchdog",
                "disk pressure on the build host",
                "host:gw-ms-a2",
                None,
                "2026-08-11T09:10:00Z",
            );
            disk.acked_at = Some(ts("2026-08-11T10:00:00Z"));

            let mut flap = attention_item(
                "att-flap",
                "watchdog",
                "kernel-facade MCP flapping",
                "mcp:kernel-facade",
                Some(3),
                "2026-08-11T09:12:00Z",
            );
            flap.muted_until = Some(ts("2026-08-11T18:00:00Z"));

            let review = attention_item(
                "att-review",
                "authority",
                "deploy blocked on grant for command c-1",
                "command/c-1",
                Some(1),
                "2026-08-11T10:25:00Z",
            );

            let mut fixed = attention_item(
                "att-fixed",
                "operator",
                "runner pool exhausted",
                "pool:runner",
                Some(2),
                "2026-08-11T09:00:00Z",
            );
            fixed.resolved_at = Some(ts("2026-08-11T11:00:00Z"));
            fixed.resolution = Some("pool widened".into());

            vec![kek, flaky, disk, flap, review, fixed]
        },
        receipts: vec![
            receipt(
                "rc-01",
                "state_flip",
                ("attempt", "at-pty-impl"),
                (Some("queued"), Some("running")),
                Some("engine reported a pid"),
                "2026-08-11T09:40:30Z",
            ),
            receipt(
                "rc-02",
                "auto_answer",
                ("gate", "g-lint"),
                (None, None),
                None,
                "2026-08-11T10:23:30Z",
            ),
            receipt(
                "rc-03",
                "issue_command",
                ("command", "c-1"),
                (None, None),
                None,
                "2026-08-11T10:24:00Z",
            ),
            receipt(
                "rc-04",
                "issue_command",
                ("command", "c-1"),
                (None, None),
                None,
                "2026-08-11T10:24:45Z",
            ),
        ],
        costs: {
            let mut ce01 = cost(
                "ce-01",
                Some("at-ship-done"),
                "codex",
                "gpt-5-codex",
                1200,
                300,
                "2026-08-11T09:00:00Z",
            );
            ce01.cost_micros = Some(CostMicros::new(340_000));
            ce01.cost_is_estimate = Some(false);

            let mut ce02 = cost(
                "ce-02",
                Some("at-pty-impl"),
                "codex",
                "gpt-5-codex",
                4200,
                900,
                "2026-08-11T09:41:00Z",
            );
            ce02.cached_input_tokens = Some(TokenCount::new(1200));

            let mut ce03 = cost(
                "ce-03",
                Some("at-pty-review"),
                "claude",
                "sonnet",
                1800,
                250,
                "2026-08-11T09:52:00Z",
            );
            ce03.cost_micros = Some(CostMicros::new(125_000));
            ce03.cost_is_estimate = Some(true);

            let mut ce04 = cost(
                "ce-04",
                Some("at-tui-impl"),
                "claude",
                "sonnet",
                5600,
                1400,
                "2026-08-11T10:16:00Z",
            );
            ce04.reasoning_tokens = Some(TokenCount::new(2100));
            ce04.cost_micros = Some(CostMicros::new(780_000));
            ce04.cost_is_estimate = Some(true);

            let mut ce05 = cost(
                "ce-05",
                Some("at-tui-queue"),
                "codex",
                "gpt-5-codex",
                300,
                0,
                "2026-08-11T10:17:00Z",
            );
            ce05.cost_micros = Some(CostMicros::new(8_000));
            ce05.cost_is_estimate = Some(false);

            let mut ce06 = cost(
                "ce-06",
                Some("at-audit-fail"),
                "claude",
                "opus",
                9000,
                2200,
                "2026-08-11T15:10:00Z",
            );
            ce06.cost_micros = Some(CostMicros::new(2_450_000));
            ce06.cost_is_estimate = Some(false);

            let ce07 = cost(
                "ce-07",
                Some("at-flaky-cancel"),
                "claude",
                "opus",
                2200,
                400,
                "2026-08-11T15:20:00Z",
            );

            let mut ce08 = cost(
                "ce-08",
                None,
                "codex",
                "gpt-5-codex",
                700,
                90,
                "2026-08-11T15:35:00Z",
            );
            ce08.dispatch_node_id = Some(DispatchNodeId::new("d-pty-recon"));
            ce08.cost_micros = Some(CostMicros::new(21_000));
            ce08.cost_is_estimate = Some(true);

            let mut ce09 = cost(
                "ce-09",
                None,
                "claude",
                "sonnet",
                3300,
                810,
                "2026-08-11T16:00:00Z",
            );
            ce09.engine_session_id = Some(EngineSessionId::new("es-tui-impl"));
            ce09.cost_micros = Some(CostMicros::new(410_000));
            ce09.cost_is_estimate = Some(false);

            let mut ce10 = cost(
                "ce-10",
                Some("at-audit-unknown"),
                "claude",
                "opus",
                1500,
                0,
                "2026-08-11T16:20:00Z",
            );
            ce10.cost_micros = Some(CostMicros::new(190_000));
            ce10.cost_is_estimate = Some(true);

            let mut ce11 = cost(
                "ce-11",
                Some("at-pty-impl"),
                "codex",
                "gpt-5-codex",
                2600,
                640,
                "2026-08-11T16:45:00Z",
            );
            ce11.cost_micros = Some(CostMicros::new(71_000));
            ce11.cost_is_estimate = Some(false);

            let mut ce12 = cost(
                "ce-12",
                Some("at-tui-impl"),
                "claude",
                "sonnet",
                6100,
                1500,
                "2026-08-11T17:20:00Z",
            );
            ce12.cost_micros = Some(CostMicros::new(905_000));
            ce12.cost_is_estimate = Some(true);

            vec![
                ce01, ce02, ce03, ce04, ce05, ce06, ce07, ce08, ce09, ce10, ce11, ce12,
            ]
        },
        sessions: vec![
            engine_session(
                "es-ship-done",
                "at-ship-done",
                "codex",
                Some("2026-08-11T09:14:00Z"),
            ),
            engine_session(
                "es-audit-fail",
                "at-audit-fail",
                "claude",
                Some("2026-08-11T15:12:00Z"),
            ),
            engine_session("es-pty-impl", "at-pty-impl", "codex", None),
            engine_session("es-tui-impl", "at-tui-impl", "claude", None),
            engine_session("es-flaky-cancel", "at-flaky-cancel", "claude", None),
        ],
        worktrees: vec![
            worktree("wt-pty", "feat/pty-lifecycle", true, true, None),
            worktree("wt-tui", "feat/7p-tui-snapshot-harness", false, true, None),
            worktree(
                "wt-release",
                "docs/release-notes",
                false,
                false,
                Some("2026-08-11T09:20:00Z"),
            ),
        ],
        leases: vec![
            lease(
                "ls-pty",
                LeaseState::Held,
                "gw-implementer",
                true,
                true,
                Some("2026-08-11T21:00:00Z"),
            ),
            lease(
                "ls-tui",
                LeaseState::Held,
                "gw-rust-pro",
                false,
                true,
                Some("2026-08-11T21:00:00Z"),
            ),
            lease(
                "ls-release",
                LeaseState::Expired,
                "gw-devrel-writer",
                false,
                false,
                Some("2026-08-11T10:00:00Z"),
            ),
        ],
        ingested: vec![
            ingested(
                "ingest:system:session-1",
                IngestionKind::Session,
                serde_json::json!({ "session": "es-tui-impl" }),
                209,
                "2026-08-11T16:00:00Z",
            ),
            ingested(
                "ingest:system:health-1",
                IngestionKind::Health,
                serde_json::json!({ "status": "ok" }),
                215,
                "2026-08-11T16:30:00Z",
            ),
        ],
        runs: {
            let mut cutover = workflow_run(
                "wfr-kernel-cutover",
                "running",
                "t-pty-lifecycle",
                "cut the pty_session lifecycle over",
                "2026-08-11T14:30:00Z",
                "2026-08-11T15:00:00Z",
            );
            cutover.step = Some("verify".into());

            let mut release_train = workflow_run(
                "wfr-release-train",
                "failed",
                "t-release-notes",
                "ship the 0.0.3 release train",
                "2026-08-11T09:00:00Z",
                "2026-08-11T17:25:00Z",
            );
            release_train.step = Some("ship".into());
            release_train.closed_at = Some(ts("2026-08-11T17:25:00Z"));

            vec![cutover, release_train]
        },
        events: {
            const STORY: &[(u64, &str, &str, &str)] = &[
                (201, "create_task", "task", "2026-08-11T08:39:00Z"),
                (202, "create_attempt", "attempt", "2026-08-11T08:45:00Z"),
                (
                    203,
                    "open_engine_session",
                    "engine_session",
                    "2026-08-11T08:50:00Z",
                ),
                (
                    204,
                    "record_cost_entry",
                    "cost_entry",
                    "2026-08-11T09:00:00Z",
                ),
                (205, "transition_attempt", "attempt", "2026-08-11T09:10:00Z"),
                (
                    206,
                    "close_engine_session",
                    "engine_session",
                    "2026-08-11T09:14:00Z",
                ),
                (207, "register_worktree", "worktree", "2026-08-11T09:20:00Z"),
                (208, "send_message", "message", "2026-08-11T09:30:00Z"),
                (209, "acquire_lease", "lease", "2026-08-11T09:40:00Z"),
                (
                    210,
                    "register_dispatch_node",
                    "dispatch_node",
                    "2026-08-11T09:45:00Z",
                ),
                (
                    211,
                    "ingest_record",
                    "ingested_record",
                    "2026-08-11T09:51:00Z",
                ),
                (
                    212,
                    "record_cost_entry",
                    "cost_entry",
                    "2026-08-11T09:52:00Z",
                ),
                (
                    213,
                    "raise_attention",
                    "attention_item",
                    "2026-08-11T10:19:00Z",
                ),
                (
                    214,
                    "open_pty_session",
                    "pty_session",
                    "2026-08-11T10:21:00Z",
                ),
                (
                    215,
                    "record_pty_attach",
                    "pty_session",
                    "2026-08-11T10:22:00Z",
                ),
                (216, "decide_gate", "gate", "2026-08-11T10:23:00Z"),
                (
                    217,
                    "ack_attention",
                    "attention_item",
                    "2026-08-11T10:35:00Z",
                ),
                (
                    218,
                    "open_workflow_run",
                    "workflow_run",
                    "2026-08-11T14:30:00Z",
                ),
                (
                    219,
                    "advance_workflow_run",
                    "workflow_run",
                    "2026-08-11T14:35:00Z",
                ),
                (220, "issue_command", "command", "2026-08-11T10:24:00Z"),
                (
                    221,
                    "record_pty_detach",
                    "pty_session",
                    "2026-08-11T17:28:00Z",
                ),
            ];
            STORY
                .iter()
                .map(|(seq, kind, aggregate, at)| {
                    event(&format!("ev-{seq}"), *seq, aggregate, kind, at)
                })
                .collect()
        },
    }
}

fn replay_recording() -> ReplayTimeline {
    let mut bytes = b"GWKREC\0\x01".to_vec();
    push_output(
        &mut bytes,
        1,
        0,
        b"$ cargo test -p gwk-tui --test seed_snapshots\r\n",
    );
    push_resize(&mut bytes, 2, 60, 100, 32);
    push_output(&mut bytes, 3, 420, b"running 21 checks\r\n");
    push_output(
        &mut bytes,
        4,
        1180,
        b"test hall_estate_at_120x40_truecolor_unicode ... ok\r\n",
    );
    ReplayTimeline::decode(&bytes).expect("valid recording bytes")
}

fn push_output(bytes: &mut Vec<u8>, seq: u64, elapsed_ms: u64, data: &[u8]) {
    bytes.extend_from_slice(&seq.to_le_bytes());
    bytes.extend_from_slice(&elapsed_ms.to_le_bytes());
    bytes.push(0);
    bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
    bytes.extend_from_slice(data);
}

fn push_resize(bytes: &mut Vec<u8>, seq: u64, elapsed_ms: u64, cols: u16, rows: u16) {
    bytes.extend_from_slice(&seq.to_le_bytes());
    bytes.extend_from_slice(&elapsed_ms.to_le_bytes());
    bytes.push(1);
    bytes.extend_from_slice(&cols.to_le_bytes());
    bytes.extend_from_slice(&rows.to_le_bytes());
}

// ---------------------------------------------------------------------------
// Board
// ---------------------------------------------------------------------------

/// The full estate as one Board panel. Every field Board reads comes off
/// the same [`raw`] facts every other lens reads a facet of.
pub fn estate_board_state(view: BoardView) -> BoardState {
    let raw = raw();
    BoardState {
        view,
        tasks: raw.tasks,
        runs: raw.runs,
        attempts: raw.attempts,
        nodes: raw.nodes,
        messages: raw.messages,
        events: raw.events,
        event_tail: EventTail {
            cursor: Some(Seq::new(200)),
            aggregate_type: None,
            event_type: None,
            live: true,
            dropped: 0,
        },
        attention: raw.attention,
        replay: replay_recording(),
        sessions: raw.sessions,
        worktrees: raw.worktrees,
        leases: raw.leases,
        costs: raw.costs,
        receipts: raw.receipts,
        ingested: raw.ingested,
        complete: true,
        watermark: Some(Seq::new(WATERMARK)),
    }
}

pub fn empty_board_state(view: BoardView) -> BoardState {
    BoardState {
        view,
        tasks: Vec::new(),
        runs: Vec::new(),
        attempts: Vec::new(),
        nodes: Vec::new(),
        messages: Vec::new(),
        events: Vec::new(),
        event_tail: EventTail::default(),
        attention: Vec::new(),
        replay: ReplayTimeline::empty(),
        sessions: Vec::new(),
        worktrees: Vec::new(),
        leases: Vec::new(),
        costs: Vec::new(),
        ingested: Vec::new(),
        receipts: Vec::new(),
        complete: true,
        watermark: None,
    }
}

// ---------------------------------------------------------------------------
// Queue
// ---------------------------------------------------------------------------

pub fn estate_queue_state() -> QueueState {
    let raw = raw();
    QueueState {
        attention: raw.attention,
        gates: raw.gates,
        receipts: raw.receipts,
        messages: raw.messages,
        watermark: Some(Seq::new(WATERMARK)),
        now: ts(NOW),
    }
}

pub fn empty_queue_state() -> QueueState {
    QueueState {
        attention: Vec::new(),
        gates: Vec::new(),
        receipts: Vec::new(),
        messages: Vec::new(),
        watermark: None,
        now: ts(NOW),
    }
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

pub fn estate_config_state() -> ConfigState {
    ConfigState {
        files: vec![
            ConfigFileState {
                path: ConfigPath::AuthorityPolicy,
                route: EditRoute::Editor,
                contents: "[[grant]]\naction_class = \"deploy\"\nscope = \"kernel\"\n".into(),
            },
            ConfigFileState {
                path: ConfigPath::Capabilities,
                route: EditRoute::Form,
                contents: "[code_write]\ndefault_agent = \"gw-rust-pro\"\n".into(),
            },
            ConfigFileState {
                path: ConfigPath::NamespaceScopes,
                route: EditRoute::Form,
                contents: "[gwk-tui]\nowner = \"gw-rust-pro\"\n".into(),
            },
            ConfigFileState {
                path: ConfigPath::Orchestration,
                route: EditRoute::Form,
                contents: "[lanes]\nsonnet = \"bounded execute\"\n".into(),
            },
        ],
        config_head: Some("a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2".into()),
        last_evidence_ref: Some("blob://sha256-configseed".into()),
        dirty: false,
        divergent: false,
    }
}

pub fn empty_config_state() -> ConfigState {
    ConfigState {
        files: Vec::new(),
        config_head: None,
        last_evidence_ref: None,
        dirty: false,
        divergent: false,
    }
}

// ---------------------------------------------------------------------------
// Drilldown
// ---------------------------------------------------------------------------

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

fn fg_style(slot: PtyAnsiSlot, bold: bool) -> CellStyle {
    CellStyle {
        bold,
        fg: Some(CellColor::Ansi16 { slot }),
        ..plain_style()
    }
}

fn text_row(width: usize, segments: &[(&str, CellStyle)]) -> Vec<StyledCell> {
    let mut cells = Vec::with_capacity(width);
    for (text, style) in segments {
        for ch in text.chars() {
            cells.push(StyledCell {
                glyph: ch.to_string(),
                style: style.clone(),
            });
        }
    }
    while cells.len() < width {
        cells.push(StyledCell {
            glyph: " ".to_owned(),
            style: plain_style(),
        });
    }
    cells.truncate(width);
    cells
}

fn plain_row(width: usize, text: &str) -> Vec<StyledCell> {
    text_row(width, &[(text, plain_style())])
}

fn blank_row(width: usize) -> Vec<StyledCell> {
    text_row(width, &[])
}

/// A plausible ~100x30 `cargo test` run attached to a live hosted PTY
/// session, driven through [`DrilldownState::ingest`] exactly as the real
/// wire would: an attach response, then a full-screen snapshot.
pub fn drilldown_attached() -> DrilldownState {
    const WIDTH: usize = 100;
    let session_id = PtySessionId::new("pty-1");
    let mut state = DrilldownState::new(session_id.clone());
    let request_id = RequestId::new("req-attach-1");
    state.begin_attach(request_id.clone());

    let attached = ServerControl::Response {
        request_id: request_id.clone(),
        result: KernelResult::PtyAttached {
            session_id: session_id.clone(),
            generation: PtySessionGeneration::new("gen-3"),
            rows: 30,
            cols: WIDTH as u16,
            cursor: None,
        },
    };
    state.ingest(&attached);

    let green = fg_style(PtyAnsiSlot::Green, false);
    let red_bold = fg_style(PtyAnsiSlot::Red, true);
    let cyan_bold = fg_style(PtyAnsiSlot::Cyan, true);

    let mut rows = vec![
        text_row(
            WIDTH,
            &[(
                "gw@kernel:~/gridwork$ cargo test -p gwk-tui --test seed_snapshots -- --nocapture",
                cyan_bold.clone(),
            )],
        ),
        blank_row(WIDTH),
        plain_row(WIDTH, "   Compiling gwk-theme v0.0.2"),
        plain_row(WIDTH, "   Compiling gwk-domain v0.0.2"),
        plain_row(WIDTH, "   Compiling gwk-tui v0.0.2 (crates/gwk-tui)"),
        blank_row(WIDTH),
        plain_row(
            WIDTH,
            "    Finished test [unoptimized + debuginfo] target(s) in 6.14s",
        ),
        plain_row(
            WIDTH,
            "     Running tests/seed_snapshots.rs (target/debug/deps/seed_snapshots-9f2a1b3c)",
        ),
        blank_row(WIDTH),
        plain_row(WIDTH, "running 21 checks"),
        text_row(
            WIDTH,
            &[
                ("test board_views_over_the_estate ", plain_style()),
                ("... ok", green.clone()),
            ],
        ),
        text_row(
            WIDTH,
            &[
                (
                    "test hall_estate_at_120x40_truecolor_unicode ",
                    plain_style(),
                ),
                ("... ok", green.clone()),
            ],
        ),
        text_row(
            WIDTH,
            &[
                ("test hall_estate_at_80x24 ", plain_style()),
                ("... FAILED", red_bold.clone()),
            ],
        ),
        text_row(
            WIDTH,
            &[
                ("test queue_estate_default ", plain_style()),
                ("... ok", green.clone()),
            ],
        ),
        text_row(
            WIDTH,
            &[
                ("test config_estate_default ", plain_style()),
                ("... ok", green),
            ],
        ),
        blank_row(WIDTH),
        plain_row(WIDTH, "failures:"),
        blank_row(WIDTH),
        plain_row(WIDTH, "---- hall_estate_at_80x24 stdout ----"),
        plain_row(
            WIDTH,
            "crates/gwk-tui/goldens/seed-hall-estate-80x24-truecolor-unicode.txt drifted at line 3.",
        ),
        blank_row(WIDTH),
        text_row(
            WIDTH,
            &[(
                "test result: FAILED. 20 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out",
                red_bold,
            )],
        ),
    ];
    while rows.len() < 29 {
        rows.push(blank_row(WIDTH));
    }
    rows.push(text_row(
        WIDTH,
        &[("gw@kernel:~/gridwork$ ", plain_style()), ("_", cyan_bold)],
    ));

    let frame = PtyFrame::from_cells(&rows);
    let snapshot = ServerControl::Response {
        request_id,
        result: KernelResult::PtySnapshot {
            session_id,
            generation: PtySessionGeneration::new("gen-3"),
            seq: PtyFrameSeq::new(42),
            frame,
        },
    };
    state.ingest(&snapshot);
    state
}
