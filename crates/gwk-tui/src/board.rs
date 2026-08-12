//! The Board lens — estate and activity summaries, workflow runs, work
//! structure, message flow, terminal replay, the running fleet, cost/health,
//! and audit receipts.
//!
//! All ten views are row surfaces over kernel facts. Work rows print state
//! as a word and use the mark cell for the graph tier; replay rows carry
//! elapsed time, event kind, and sequence from the typed recording reader
//! without inventing another graph-tier mark.
//!
//! # The fleet and cost/health panels read the log and nothing else
//!
//! Both new panels render kernel projections only — engine sessions,
//! worktrees, leases, and the attempt/dispatch counts for the fleet;
//! `CostEntry` and the `health`/`session` ingestion kinds for cost/health.
//! No estate telemetry producer is re-pointed at the log to feed them, so
//! both are thinner than a dashboard reading a purpose-built metrics store,
//! and that is priced rather than hidden.
//!
//! What makes them honest instead of merely thin is [`UnknownNote`]: **what
//! is not in the log is not on the panel, and the panel says so** rather
//! than painting a blank or a zero. Three absences recur and each has words:
//! an engine session with no end stamp is *unended*, which is not the same
//! claim as alive; a `CostEntry` may carry token counts with no currency at
//! all, because the ledger never converts on an engine's behalf; and an
//! ingestion kind with no rows may have no producer rather than no activity.
//! A blank cell cannot tell those apart from a value of zero, so none of
//! them is ever drawn blank.
//!
//! # A fold over a partial read is a floor, not a total
//!
//! The kernel offers no server-side aggregation, no time ordering, and no
//! filter by ingestion kind: every figure here is a client-side fold over
//! id-ordered projection pages. So a caller that stopped short of the last
//! page hands this lens a prefix of the ledger, and a sum over a prefix is
//! not a sum. [`BoardState::complete`] carries that fact, and every folded
//! figure changes its own word — `total` against a complete read, `at least`
//! against a partial one. The lens never guesses which it got.
//!
//! # The DAG is layered, and the layers are indentation
//!
//! The wire has no task-to-task edge: a task fans out to attempts
//! (`attempt.task_id`) and each attempt roots a spawn tree
//! (`dispatch_node.attempt_id` / `parent_id`). That makes the work graph a
//! forest, and a forest drawn parent-above-child with one indent step per
//! layer is a layered drawing with zero edge crossings — the layered
//! default at its cheapest. Node labels stay short; the full fields live in
//! the selection detail pane, which is the ruled answer to labels that do
//! not fit (a detail pane, not bigger labels).
//!
//! # The flow view threads replies; arrival lives in the Queue
//!
//! Messages thread by `reply_to` under the root's `correlation_id` group.
//! This view is never the only place a message fact exists: arrival's
//! standing row is the Queue's, so an operator with the Board closed — or
//! motion off — loses nothing they must see.
//!
//! # No gauges
//!
//! Attempt detail states budget caps (`Attempt::budget`) but does not join the
//! separately projected `CostEntry` usage rows from the cost panel. A gauge in
//! attempt detail would therefore fabricate a local numerator; caps and usage
//! remain separate facts rather than a ratio the contract does not project.
//!
//! # Absent parents are said in words
//!
//! Pagination can hand this lens an attempt whose task is beyond the page,
//! or a spawn whose anchor is. Those rows render under an `off-page` header
//! rather than vanishing, and a parent cycle in the wire data is counted,
//! never followed: absence is a fact, and facts get words.

use gwk_domain::command::KernelCommand;
use gwk_domain::entity::{
    Attempt, AttentionItem, Budget, CostEntry, DispatchNode, EngineSession, IngestedRecord, Lease,
    Message, Receipt, Task, WorkflowRun, Worktree,
};
use gwk_domain::envelope::EventEnvelope;
use gwk_domain::fsm::{AttemptState, LeaseState, MessageState, TaskState};
use gwk_domain::ids::{
    AttemptId, AttentionItemId, CommandId, CostEntryId, DispatchNodeId, EngineSessionId, EventId,
    IngestedRecordId, LeaseId, MessageId, ReceiptId, Seq, TaskId, Timestamp, WorkflowRunId,
    WorktreeId,
};
use gwk_domain::ingestion::IngestionKind;
use gwk_theme::marks::{GlyphSet, Mark, StateBinding};
use gwk_theme::tier::ColorTier;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use crate::input::HitMap;
use crate::replay::{ReplayFrame, ReplayTimeline};
use crate::theme;

/// Which Board panel the frame shows. One lens, ten views, one visible at
/// a time — the others are a keystroke away, never extra panes fighting for
/// the same columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BoardView {
    #[default]
    Estate,
    Activity,
    Runs,
    Dag,
    Flow,
    Events,
    Replay,
    Fleet,
    CostHealth,
    Audit,
}

impl BoardView {
    /// Every panel in the Board's stable navigation order — the order
    /// [`Self::next`] walks, and the order the status bar's tab strip prints.
    pub const ALL: [Self; 10] = [
        Self::Estate,
        Self::Activity,
        Self::Runs,
        Self::Dag,
        Self::Flow,
        Self::Events,
        Self::Replay,
        Self::Fleet,
        Self::CostHealth,
        Self::Audit,
    ];

    /// The tab strip's name for this panel.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Estate => "estate",
            Self::Activity => "brief",
            Self::Runs => "run",
            Self::Dag => "dag",
            Self::Flow => "flow",
            Self::Events => "events",
            Self::Replay => "replay",
            Self::Fleet => "fleet",
            Self::CostHealth => "cost",
            Self::Audit => "audit",
        }
    }

    /// The next panel in the Board's stable navigation order.
    pub const fn next(self) -> Self {
        match self {
            Self::Estate => Self::Activity,
            Self::Activity => Self::Runs,
            Self::Runs => Self::Dag,
            Self::Dag => Self::Flow,
            Self::Flow => Self::Events,
            Self::Events => Self::Replay,
            Self::Replay => Self::Fleet,
            Self::Fleet => Self::CostHealth,
            Self::CostHealth => Self::Audit,
            Self::Audit => Self::Estate,
        }
    }

    /// The previous panel in the Board's stable navigation order.
    pub const fn previous(self) -> Self {
        match self {
            Self::Estate => Self::Audit,
            Self::Activity => Self::Estate,
            Self::Runs => Self::Activity,
            Self::Dag => Self::Runs,
            Self::Flow => Self::Dag,
            Self::Events => Self::Flow,
            Self::Replay => Self::Events,
            Self::Fleet => Self::Replay,
            Self::CostHealth => Self::Fleet,
            Self::Audit => Self::CostHealth,
        }
    }
}

/// The operator's position and exact-match filters for the event tail. The
/// cursor remains the requested starting point while `BoardState::watermark`
/// advances with delivered batches.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EventTail {
    pub cursor: Option<Seq>,
    pub aggregate_type: Option<String>,
    pub event_type: Option<String>,
    pub live: bool,
    /// Older delivered rows removed from the bounded in-memory tail.
    pub dropped: usize,
}

/// Everything the lens paints, assembled by the caller from projection
/// pages. The lens never fetches: it is a pure function of this value.
#[derive(Debug, Clone)]
pub struct BoardState {
    pub view: BoardView,
    pub tasks: Vec<Task>,
    /// Workflow choreography as a durable run ledger. Closed rows remain
    /// history; `step` is printed as opaque template data.
    pub runs: Vec<WorkflowRun>,
    pub attempts: Vec<Attempt>,
    pub nodes: Vec<DispatchNode>,
    pub messages: Vec<Message>,
    /// Immutable event facts retained by the bounded tail view.
    pub events: Vec<EventEnvelope>,
    pub event_tail: EventTail,
    /// Current attention facts used by the estate head and activity debt.
    pub attention: Vec<AttentionItem>,
    /// The persisted recording selected for the replay panel.
    pub replay: ReplayTimeline,
    /// Provider-level sessions under attempts — the fleet panel's live rows.
    pub sessions: Vec<EngineSession>,
    /// Isolated working copies, held or handed back.
    pub worktrees: Vec<Worktree>,
    /// Advisory leases over worktrees, file sets, and singleton roles.
    pub leases: Vec<Lease>,
    /// Usage rows for the cost panel. Append-only spend facts, never an
    /// accumulator: the total is this fold and nothing else.
    pub costs: Vec<CostEntry>,
    /// The attestation ledger the audit panel reads. One row per action an
    /// actor performed, minted by the kernel in the same transaction as the
    /// action itself.
    pub receipts: Vec<Receipt>,
    /// Ingested records for the health and session halves. One projection
    /// carries all twelve kinds and the wire offers no filter, so the panel
    /// selects by [`IngestedRecord::kind`] here rather than at the socket.
    pub ingested: Vec<IngestedRecord>,
    /// Whether the caller paged every projection this frame folds to
    /// exhaustion. `false` makes every folded figure a FLOOR — the panels
    /// print `at least` instead of `total`, because a sum over a prefix of
    /// an id-ordered ledger is not a sum, and the lens cannot tell from the
    /// rows alone which one it was handed.
    pub complete: bool,
    /// The projector's as-of stamp from the page this state was read at.
    pub watermark: Option<Seq>,
}

/// What a Board row stands for — the target a click or a keystroke acts on.
/// Every target opens the detail pane. Attempt targets additionally support
/// the narrow command builders below; every other target remains read-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoardTarget {
    Task(TaskId),
    WorkflowRun(WorkflowRunId),
    Attempt(AttemptId),
    Node(DispatchNodeId),
    Message(MessageId),
    Event(EventId),
    Attention(AttentionItemId),
    ReplayFrame(u64),
    Session(EngineSessionId),
    Worktree(WorktreeId),
    Lease(LeaseId),
    Cost(CostEntryId),
    Ingested(IngestedRecordId),
    Receipt(ReceiptId),
}

/// Build the one targeted stop the command contract carries.
///
/// `stop_attempt` is the whole existing surface; the client models no inverse.
pub fn stop_attempt(target: &BoardTarget) -> Option<KernelCommand> {
    let BoardTarget::Attempt(id) = target else {
        return None;
    };
    Some(KernelCommand::IssueCommand {
        command_id: CommandId::new(format!("stop-attempt:{}", id.as_str())),
        kind: "stop_attempt".to_owned(),
        targets: vec![id.as_str().to_owned()],
        actor: None,
    })
}

/// Replace the selected attempt's four-axis budget at the version the Board
/// actually read. A stale selection has no version to invent and yields no act.
pub fn update_attempt_budget(
    state: &BoardState,
    target: &BoardTarget,
    budget: Budget,
) -> Option<KernelCommand> {
    let BoardTarget::Attempt(id) = target else {
        return None;
    };
    let attempt = state
        .attempts
        .iter()
        .find(|attempt| attempt.id.as_str() == id.as_str())?;
    Some(replace_attempt_budget(id.clone(), attempt.version, budget))
}

/// Build the same replacement command when the caller already holds the
/// attempt id and CAS version, as the CLI twin does.
pub fn replace_attempt_budget(
    attempt_id: AttemptId,
    expected_version: u32,
    budget: Budget,
) -> KernelCommand {
    KernelCommand::UpdateBudget {
        attempt_id,
        expected_version,
        budget,
    }
}

/// Counts in the one-screen estate view. Against a partial page these are
/// floors; [`EstateOverview::complete`] carries that distinction to both twins.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EstateCounts {
    pub tasks: usize,
    pub active_tasks: usize,
    pub attempts: usize,
    pub running_attempts: usize,
    pub unresolved_attention: usize,
    pub held_worktrees: usize,
    pub held_leases: usize,
}

/// The first unresolved attention item in the contract's P0-first order.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AttentionHead {
    pub id: AttentionItemId,
    pub summary: String,
    pub priority: Option<i32>,
    pub subject_ref: Option<String>,
    pub raised_at: Timestamp,
    pub acked_at: Option<Timestamp>,
    pub muted_until: Option<Timestamp>,
}

/// One recorded change shown by both summary verbs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ActivityFact {
    pub at: Timestamp,
    pub kind: String,
    pub id: String,
    pub summary: String,
}

/// One current fact the activity brief says remains owed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct OwedFact {
    pub kind: String,
    pub id: String,
    pub reason: String,
}

/// The priced subset of the cost ledger. `cost_micros` is decimal text because
/// a fold may exceed JSON's exact integer range; absent means no row named cost.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CostHeadline {
    pub entries: usize,
    pub priced_entries: usize,
    pub unpriced_entries: usize,
    pub estimated_entries: usize,
    pub cost_micros: Option<String>,
}

/// One engine/model bucket in the typed cost fold.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EngineCostRollup {
    pub engine: String,
    pub model: Option<String>,
    pub entries: usize,
    pub priced_entries: usize,
    pub cost_micros: Option<String>,
}

/// One token axis, preserving how many ledger rows reported it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TokenCoverage {
    pub total: Option<String>,
    pub reported_entries: usize,
}

/// The five token columns the cost-entry contract carries.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CostTokens {
    pub input: TokenCoverage,
    pub output: TokenCoverage,
    pub cached_input: TokenCoverage,
    pub cache_write: TokenCoverage,
    pub reasoning: TokenCoverage,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CostUnknown {
    pub subject: &'static str,
    pub why: String,
}

/// The typed machine twin of the Board's cost fold.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CostRollup {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub complete: bool,
    pub watermark: Option<Seq>,
    pub headline: CostHeadline,
    pub by_engine: Vec<EngineCostRollup>,
    pub tokens: CostTokens,
    /// Newest first among the rows the caller supplied.
    pub entries: Vec<CostEntry>,
    pub findings: Vec<String>,
    pub unknowns: Vec<CostUnknown>,
}

/// One attempt's recorded budget and the axes the contract leaves uncapped.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AttemptBudget {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub attempt_id: AttemptId,
    pub version: u32,
    pub budget: Option<Budget>,
    pub uncapped_axes: Vec<&'static str>,
}

/// State now: counts, the attention head, and the newest facts on the pages.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EstateOverview {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub complete: bool,
    pub watermark: Option<Seq>,
    pub counts: EstateCounts,
    pub attention_head: Option<AttentionHead>,
    pub recent_activity: Vec<ActivityFact>,
    pub findings: Vec<String>,
    pub unknowns: Vec<String>,
}

/// Delta intent over projection facts: what changed most recently and what
/// remains open. It names no time window the contract did not provide.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ActivityBrief {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub complete: bool,
    pub watermark: Option<Seq>,
    pub happened: Vec<ActivityFact>,
    pub owed_total: usize,
    pub owed: Vec<OwedFact>,
    pub cost: CostHeadline,
    pub findings: Vec<String>,
    pub unknowns: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FleetCounts {
    pub sessions: usize,
    pub unended_sessions: usize,
    pub running_attempts: usize,
    pub attempts_without_session: usize,
    pub spawns: usize,
    pub held_worktrees: usize,
    pub held_leases: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FleetUnknown {
    pub subject: &'static str,
    pub why: String,
}

/// The typed machine twin of the Board fleet panel.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AgentFleet {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub complete: bool,
    pub watermark: Option<Seq>,
    pub counts: FleetCounts,
    pub sessions: Vec<EngineSession>,
    pub dispatch_nodes: Vec<DispatchNode>,
    pub worktrees: Vec<Worktree>,
    pub leases: Vec<Lease>,
    pub findings: Vec<String>,
    pub unknowns: Vec<FleetUnknown>,
}

/// A tree deeper than this stops indenting further. Rows retain their logical
/// depth so the painter can state the real level after the capped indent.
const INDENT_CAP: u16 = 6;

/// Cells per layer of the DAG.
const INDENT_STEP: u16 = 2;

/// Raw output bytes shown in a replay row or detail preview. The frame still
/// owns every byte; presentation stays bounded independently of pane width.
const OUTPUT_PREVIEW_BYTES: usize = 64;

/// `HH:MM` out of an RFC 3339 timestamp, for the compact stamps rows carry.
fn hhmm(ts: &Timestamp) -> &str {
    ts.as_str().get(11..16).unwrap_or(ts.as_str())
}

/// The Board prints the state as a WORD — rows here have the room the Hall
/// does not — and the binding supplies colour and nothing else. Several
/// states deliberately share a token: colour carries the axis (is this
/// fine, waiting, or wrong), the word carries the identity.
fn task_face(state: TaskState) -> (&'static str, &'static StateBinding) {
    match state {
        TaskState::Submitted => ("submitted", theme::binding("queued")),
        TaskState::Working => ("working", theme::binding("running")),
        TaskState::InputRequired => ("input required", theme::binding("needs_attention")),
        TaskState::Completed => ("completed", theme::binding("done")),
        TaskState::Failed => ("failed", theme::binding("failed")),
        TaskState::Canceled => ("canceled", theme::binding("canceled")),
    }
}

fn attempt_face(state: AttemptState) -> (&'static str, &'static StateBinding) {
    match state {
        AttemptState::Queued => ("queued", theme::binding("queued")),
        // Leased is still waiting on an engine — muted like queued; the
        // word is what separates them.
        AttemptState::Leased => ("leased", theme::binding("queued")),
        AttemptState::Starting => ("starting", theme::binding("starting")),
        AttemptState::Running => ("running", theme::binding("running")),
        AttemptState::Blocked => ("blocked", theme::binding("blocked")),
        AttemptState::Canceling => ("canceling", theme::binding("canceling")),
        AttemptState::Canceled => ("canceled", theme::binding("canceled")),
        AttemptState::Failed => ("failed", theme::binding("failed")),
        // Unknown is a first-class terminal, never coerced to failed — the
        // warn token says "look", not "it broke".
        AttemptState::Unknown => ("unknown", theme::binding("unknown")),
        AttemptState::Succeeded => ("succeeded", theme::binding("done")),
    }
}

fn message_face(state: MessageState) -> (&'static str, &'static StateBinding) {
    match state {
        MessageState::Accepted => ("accepted", theme::binding("queued")),
        MessageState::Delivered => ("delivered", theme::binding("done")),
        MessageState::Acknowledged => ("acknowledged", theme::binding("done")),
        MessageState::Applied => ("applied", theme::binding("done")),
        MessageState::Rejected => ("rejected", theme::binding("failed")),
        MessageState::DeadLetter => ("dead letter", theme::binding("failed")),
    }
}

fn graph_mark(name: &str) -> &'static Mark {
    gwk_theme::marks::mark(name).expect("the graph-tier marks are pinned")
}

/// The board's ambient pulse: attempts running right now. The DAG summary
/// and the status bar both derive from this single walk, so they cannot
/// disagree.
fn running_count(state: &BoardState) -> usize {
    let mut seen = std::collections::BTreeSet::new();
    state
        .attempts
        .iter()
        .filter(|attempt| seen.insert(attempt.id.as_str()))
        .filter(|attempt| attempt.state == AttemptState::Running)
        .count()
}

/// One row's paint: layer, mark cell, styled text, and what it stands for.
struct Row {
    /// The logical layer, in indent cells before the mark. Paint caps this
    /// without discarding the real depth.
    indent: u16,
    /// The graph-tier mark and the style its cell keeps even when the row
    /// is selected — the state colour is part of the row's meaning.
    mark: Option<(&'static Mark, Style)>,
    text: String,
    /// A right-aligned tail, painted only when it clears the text —
    /// dropped, not squeezed, in a pane too narrow to hold both.
    right: Option<String>,
    style: Style,
    target: Option<BoardTarget>,
    diagnostic: bool,
}

impl Row {
    fn plain(text: String, style: Style) -> Self {
        Row {
            indent: 0,
            mark: None,
            text,
            right: None,
            style,
            target: None,
            diagnostic: false,
        }
    }

    fn diagnostic(text: String, style: Style) -> Self {
        Row {
            diagnostic: true,
            ..Self::plain(text, style)
        }
    }
}

/// Byte-ordered ids, the same order the wire's `COLLATE "C"` pagination
/// walks — the DAG needs no cleverer sibling order for zero crossings,
/// because a forest drawn depth-first never crosses at all.
fn by_id<'a, T, F: Fn(&T) -> &str>(items: impl Iterator<Item = &'a T>, id: F) -> Vec<&'a T> {
    let mut out: Vec<&T> = items.collect();
    out.sort_by(|a, b| id(a).cmp(id(b)));
    out
}

/// Keep the first row for each projection id and count overlapping/invalid
/// duplicates so the frame can name them instead of duplicating targets.
fn unique_by_id<'a, T>(items: &'a [T], id: impl Fn(&'a T) -> &'a str) -> (Vec<&'a T>, usize) {
    let mut seen = std::collections::BTreeSet::new();
    let mut unique = Vec::with_capacity(items.len());
    let mut duplicates = 0;
    for item in items {
        if seen.insert(id(item)) {
            unique.push(item);
        } else {
            duplicates += 1;
        }
    }
    (unique, duplicates)
}

fn task_row(task: &Task, tier: ColorTier) -> Row {
    let (word, bind) = task_face(task.state);
    let title = task.title.as_deref().unwrap_or_else(|| task.id.as_str());
    Row {
        indent: 0,
        mark: Some((graph_mark("task"), theme::state_style(bind, tier))),
        text: format!("{title}  {word}"),
        right: Some(hhmm(&task.updated_at).to_string()),
        style: theme::state_style(bind, tier),
        target: Some(BoardTarget::Task(task.id.clone())),
        diagnostic: false,
    }
}

fn attempt_row(attempt: &Attempt, indent: u16, tier: ColorTier) -> Row {
    let (word, bind) = attempt_face(attempt.state);
    let mut text = format!("{}  {word}", attempt.engine.as_str());
    if let Some(role) = &attempt.role {
        text.push_str(&format!("  ({role})"));
    }
    Row {
        indent,
        mark: Some((graph_mark("attempt"), theme::state_style(bind, tier))),
        text,
        right: Some(hhmm(&attempt.updated_at).to_string()),
        style: theme::state_style(bind, tier),
        target: Some(BoardTarget::Attempt(attempt.id.clone())),
        diagnostic: false,
    }
}

fn node_row(node: &DispatchNode, indent: u16, muted: Style) -> Row {
    // The spawn label is an OPEN string, not an FSM state, so no colour
    // pretends to understand it: the word is printed as the wire said it.
    let label = node.label.as_deref().unwrap_or_else(|| node.id.as_str());
    Row {
        indent,
        mark: Some((graph_mark("dispatch"), muted)),
        text: format!("{label}  {}  ({})", node.state, node.kind),
        right: None,
        style: muted,
        target: Some(BoardTarget::Node(node.id.clone())),
        diagnostic: false,
    }
}

type SpawnChildren<'a> =
    std::collections::BTreeMap<(Option<&'a str>, &'a str), Vec<&'a DispatchNode>>;

/// Walk one attempt's spawn tree depth-first with an explicit stack — wire
/// data is not trusted to be acyclic — marking every node reached.
fn spawn_rows<'a>(
    roots: Vec<&'a DispatchNode>,
    children_by_parent: &SpawnChildren<'a>,
    placed: &mut std::collections::BTreeSet<&'a str>,
    base_indent: u16,
    muted: Style,
    out: &mut Vec<Row>,
) {
    let mut stack: Vec<(&DispatchNode, u16)> = Vec::new();
    for root in roots.into_iter().rev() {
        stack.push((root, base_indent));
    }
    while let Some((node, indent)) = stack.pop() {
        if !placed.insert(node.id.as_str()) {
            // Already placed: a parent cycle reached the same row twice.
            // Duplicate ids were normalized before this walk.
            continue;
        }
        out.push(node_row(node, indent, muted));
        let key = (
            node.attempt_id.as_ref().map(|attempt| attempt.as_str()),
            node.id.as_str(),
        );
        if let Some(children) = children_by_parent.get(&key) {
            for child in children.iter().rev() {
                stack.push((child, indent.saturating_add(INDENT_STEP)));
            }
        }
    }
}

/// Build the DAG view's rows. Pure, and the whole layout: the painter only
/// places what this returns.
fn dag_rows(state: &BoardState, tier: ColorTier) -> Vec<Row> {
    let muted = theme::state_style(theme::binding("idle"), tier);
    let mut out = Vec::new();

    if state.tasks.is_empty() && state.attempts.is_empty() && state.nodes.is_empty() {
        out.push(Row::plain(
            "nothing on the board -- no work dispatched".into(),
            muted,
        ));
        return out;
    }

    let (tasks, duplicate_tasks) = unique_by_id(&state.tasks, |task| task.id.as_str());
    let (attempts, duplicate_attempts) =
        unique_by_id(&state.attempts, |attempt| attempt.id.as_str());
    let (nodes, duplicate_nodes) = unique_by_id(&state.nodes, |node| node.id.as_str());
    let duplicate_count = duplicate_tasks + duplicate_attempts + duplicate_nodes;

    let mut attempts_by_task: std::collections::BTreeMap<&str, Vec<&Attempt>> =
        std::collections::BTreeMap::new();
    for attempt in &attempts {
        attempts_by_task
            .entry(attempt.task_id.as_str())
            .or_default()
            .push(attempt);
    }
    for task_attempts in attempts_by_task.values_mut() {
        task_attempts.sort_by(|left, right| left.id.as_str().cmp(right.id.as_str()));
    }

    out.push(Row::plain(
        format!("{} tasks  {} running", tasks.len(), running_count(state)),
        muted,
    ));

    let task_ids: std::collections::BTreeSet<&str> = tasks.iter().map(|t| t.id.as_str()).collect();
    let mut placed_nodes: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();

    // A node roots a subtree when it has no parent on this page; it anchors
    // under its attempt when that attempt is on the page at all.
    let node_ids: std::collections::BTreeSet<&str> = nodes.iter().map(|n| n.id.as_str()).collect();
    let node_anchors: std::collections::BTreeSet<(Option<&str>, &str)> = nodes
        .iter()
        .map(|node| {
            (
                node.attempt_id.as_ref().map(|attempt| attempt.as_str()),
                node.id.as_str(),
            )
        })
        .collect();
    let mut children_by_parent: SpawnChildren<'_> = std::collections::BTreeMap::new();
    for node in &nodes {
        if let Some(parent) = &node.parent_id {
            children_by_parent
                .entry((
                    node.attempt_id.as_ref().map(|attempt| attempt.as_str()),
                    parent.as_str(),
                ))
                .or_default()
                .push(node);
        }
    }
    for children in children_by_parent.values_mut() {
        children.sort_by(|left, right| left.id.as_str().cmp(right.id.as_str()));
    }
    let parent_in_same_attempt = |node: &DispatchNode| {
        node.parent_id.as_ref().is_some_and(|parent_id| {
            node_anchors.contains(&(
                node.attempt_id.as_ref().map(|attempt| attempt.as_str()),
                parent_id.as_str(),
            ))
        })
    };
    let conflicting_anchors = nodes
        .iter()
        .filter(|node| {
            node.parent_id
                .as_ref()
                .is_some_and(|parent| node_ids.contains(parent.as_str()))
                && !parent_in_same_attempt(node)
        })
        .count();
    let mut roots_by_attempt: std::collections::BTreeMap<Option<&str>, Vec<&DispatchNode>> =
        std::collections::BTreeMap::new();
    for node in &nodes {
        if !parent_in_same_attempt(node) {
            roots_by_attempt
                .entry(node.attempt_id.as_ref().map(|attempt| attempt.as_str()))
                .or_default()
                .push(node);
        }
    }
    for roots in roots_by_attempt.values_mut() {
        roots.sort_by(|left, right| left.id.as_str().cmp(right.id.as_str()));
    }
    let roots_of = |attempt_id: &str| -> Vec<&DispatchNode> {
        roots_by_attempt
            .get(&Some(attempt_id))
            .cloned()
            .unwrap_or_default()
    };

    let mut place_attempt = |attempt: &Attempt, indent: u16, out: &mut Vec<Row>| {
        out.push(attempt_row(attempt, indent, tier));
        spawn_rows(
            roots_of(attempt.id.as_str()),
            &children_by_parent,
            &mut placed_nodes,
            indent + INDENT_STEP,
            muted,
            out,
        );
    };

    for task in by_id(tasks.iter().copied(), |t| t.id.as_str()) {
        out.push(task_row(task, tier));
        if let Some(task_attempts) = attempts_by_task.get(task.id.as_str()) {
            for attempt in task_attempts {
                place_attempt(attempt, INDENT_STEP, &mut out);
            }
        }
    }

    // Off-page: attempts whose task is beyond the page, and spawns anchored
    // to nothing on it. Rendered, headed, never dropped.
    let orphan_attempts = by_id(
        attempts
            .iter()
            .copied()
            .filter(|a| !task_ids.contains(a.task_id.as_str())),
        |a| a.id.as_str(),
    );
    let attempt_ids: std::collections::BTreeSet<&str> =
        attempts.iter().map(|a| a.id.as_str()).collect();
    let orphan_node_roots = roots_by_attempt
        .iter()
        .filter(|(attempt_id, _)| {
            !attempt_id.is_some_and(|attempt_id| attempt_ids.contains(attempt_id))
        })
        .flat_map(|(_, roots)| roots.iter().copied())
        .collect::<Vec<_>>();
    if !orphan_attempts.is_empty() || !orphan_node_roots.is_empty() {
        out.push(Row::plain(
            "off-page -- parents beyond this page".into(),
            muted,
        ));
        for attempt in orphan_attempts {
            place_attempt(attempt, INDENT_STEP, &mut out);
        }
        spawn_rows(
            orphan_node_roots,
            &children_by_parent,
            &mut placed_nodes,
            INDENT_STEP,
            muted,
            &mut out,
        );
    }

    // Anything still unplaced sits in a parent cycle no root reaches. Said
    // in words, with the count, in the same frame that could not draw it.
    let unplaced = nodes
        .iter()
        .filter(|n| !placed_nodes.contains(n.id.as_str()))
        .count();
    let mut diagnostics = Vec::new();
    if unplaced > 0 {
        diagnostics.push(format!("+{unplaced} unplaced -- parent cycle"));
    }
    if conflicting_anchors > 0 {
        let noun = if conflicting_anchors == 1 {
            "anchor"
        } else {
            "anchors"
        };
        diagnostics.push(format!(
            "+{conflicting_anchors} conflicting {noun} -- invalid page"
        ));
    }
    if duplicate_count > 0 {
        let noun = if duplicate_count == 1 { "id" } else { "ids" };
        diagnostics.push(format!(
            "+{duplicate_count} duplicate {noun} -- invalid page"
        ));
    }
    if !diagnostics.is_empty() {
        let count = diagnostics.len();
        for diagnostic in diagnostics.into_iter().rev() {
            out.insert(0, Row::diagnostic(diagnostic, muted));
        }
        out.insert(
            0,
            Row::diagnostic(format!("invalid page -- {count} findings"), muted),
        );
    }

    out
}

fn workflow_run_row(run: &WorkflowRun, tier: ColorTier) -> Row {
    let live = run.closed_at.is_none();
    let binding = if live {
        theme::binding("running")
    } else {
        match run.state.as_str() {
            "completed" => theme::binding("done"),
            "failed" => theme::binding("failed"),
            "canceled" => theme::binding("canceled"),
            _ => theme::binding("unknown"),
        }
    };
    let label = run.title.as_deref().unwrap_or_else(|| run.id.as_str());
    // The step is template data. Print the wire value without assigning it an
    // icon, order, or vocabulary; a missing value is equally explicit.
    let step = run
        .step
        .as_deref()
        .map_or_else(|| "step absent".to_owned(), |step| format!("step {step}"));
    let text = if live {
        format!("{label}  {}  {step}", run.state)
    } else {
        format!("{label}  outcome {}  {step}", run.state)
    };
    let right = run.closed_at.as_ref().map_or_else(
        || format!("updated {}", hhmm(&run.updated_at)),
        |closed| format!("closed {}", hhmm(closed)),
    );
    Row {
        indent: INDENT_STEP,
        mark: None,
        text,
        right: Some(right),
        style: theme::state_style(binding, tier),
        target: Some(BoardTarget::WorkflowRun(run.id.clone())),
        diagnostic: false,
    }
}

/// The workflow ledger, split into the runs still moving and immutable close
/// history. The split is derived from `closed_at`; the step remains opaque.
fn workflow_run_rows(state: &BoardState, tier: ColorTier) -> Vec<Row> {
    let muted = theme::state_style(theme::binding("idle"), tier);
    let (mut runs, duplicate_runs) = unique_by_id(&state.runs, |run| run.id.as_str());
    runs.sort_by(|left, right| {
        right
            .updated_at
            .as_str()
            .cmp(left.updated_at.as_str())
            .then_with(|| left.id.as_str().cmp(right.id.as_str()))
    });

    let findings = duplicate_finding(duplicate_runs, "workflow run")
        .into_iter()
        .collect();
    let unknowns = (!state.complete)
        .then(|| UnknownNote {
            subject: "run ledger",
            why: "read short of the last page -- rows are a prefix".to_owned(),
        })
        .into_iter()
        .collect();
    let mut out = pinned_block(findings, unknowns, muted);
    if runs.is_empty() {
        out.push(Row::plain(
            "no workflow runs -- no run history on this page".to_owned(),
            muted,
        ));
        return out;
    }

    out.push(Row::plain(
        format!(
            "{}  {}",
            plural(runs.len(), "workflow run", "workflow runs"),
            fold_word(state.complete),
        ),
        muted,
    ));
    let (live, closed): (Vec<_>, Vec<_>) =
        runs.into_iter().partition(|run| run.closed_at.is_none());
    if !live.is_empty() {
        out.push(Row::plain("live".to_owned(), muted));
        out.extend(live.into_iter().map(|run| workflow_run_row(run, tier)));
    }
    if !closed.is_empty() {
        out.push(Row::plain("history".to_owned(), muted));
        out.extend(closed.into_iter().map(|run| workflow_run_row(run, tier)));
    }
    out
}

/// Build the flow view's rows: replies threaded under their parent, roots
/// grouped by correlation, uncorrelated roots standing alone.
fn flow_rows(state: &BoardState, tier: ColorTier) -> Vec<Row> {
    let muted = theme::state_style(theme::binding("idle"), tier);
    let mut out = Vec::new();

    if state.messages.is_empty() {
        out.push(Row::plain("no message flows -- nothing sent".into(), muted));
        return out;
    }

    let (messages, duplicate_count) = unique_by_id(&state.messages, |message| message.id.as_str());
    let message_ids: std::collections::BTreeSet<&str> =
        messages.iter().map(|m| m.id.as_str()).collect();
    // A message roots a thread when its reply target is not on this page —
    // a lost parent makes a root, never a dropped row.
    let roots: Vec<&Message> = messages
        .iter()
        .copied()
        .filter(|m| {
            !m.reply_to
                .as_ref()
                .is_some_and(|p| message_ids.contains(p.as_str()))
        })
        .collect();

    let mut correlated: std::collections::BTreeMap<&str, Vec<&Message>> =
        std::collections::BTreeMap::new();
    let mut standalone: Vec<&Message> = Vec::new();
    for root in roots {
        match &root.correlation_id {
            Some(cid) => correlated.entry(cid.as_str()).or_default().push(root),
            None => standalone.push(root),
        }
    }
    let flows = correlated.len() + standalone.len();
    out.push(Row::plain(
        format!("{flows} flows  {} messages", messages.len()),
        muted,
    ));

    let arrival_order = |a: &&Message, b: &&Message| {
        a.created_at
            .as_str()
            .cmp(b.created_at.as_str())
            .then_with(|| a.id.as_str().cmp(b.id.as_str()))
    };
    let mut replies_by_parent: std::collections::BTreeMap<&str, Vec<&Message>> =
        std::collections::BTreeMap::new();
    for message in &messages {
        if let Some(parent) = &message.reply_to {
            replies_by_parent
                .entry(parent.as_str())
                .or_default()
                .push(message);
        }
    }
    for replies in replies_by_parent.values_mut() {
        replies.sort_by(arrival_order);
    }

    let mut placed: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let thread = |root: &Message,
                  base_indent: u16,
                  placed: &mut std::collections::BTreeSet<String>,
                  out: &mut Vec<Row>| {
        let mut stack: Vec<(&Message, u16)> = vec![(root, base_indent)];
        while let Some((message, indent)) = stack.pop() {
            if !placed.insert(message.id.as_str().to_string()) {
                continue;
            }
            out.push(message_row(message, indent, tier));
            if let Some(replies) = replies_by_parent.get(message.id.as_str()) {
                for reply in replies.iter().rev() {
                    stack.push((reply, indent.saturating_add(INDENT_STEP)));
                }
            }
        }
    };

    for (cid, mut group) in correlated {
        out.push(Row::plain(format!("flow {cid}"), muted));
        group.sort_by(arrival_order);
        for root in group {
            thread(root, INDENT_STEP, &mut placed, &mut out);
        }
    }
    standalone.sort_by(arrival_order);
    for root in standalone {
        thread(root, 0, &mut placed, &mut out);
    }

    let unthreaded = messages
        .iter()
        .filter(|message| !placed.contains(message.id.as_str()))
        .count();
    let mut diagnostics = Vec::new();
    if unthreaded > 0 {
        diagnostics.push(format!("+{unthreaded} unthreaded -- reply cycle"));
    }
    if duplicate_count > 0 {
        let noun = if duplicate_count == 1 { "id" } else { "ids" };
        diagnostics.push(format!(
            "+{duplicate_count} duplicate {noun} -- invalid page"
        ));
    }
    if !diagnostics.is_empty() {
        let count = diagnostics.len();
        for diagnostic in diagnostics.into_iter().rev() {
            out.insert(0, Row::diagnostic(diagnostic, muted));
        }
        out.insert(
            0,
            Row::diagnostic(format!("invalid page -- {count} findings"), muted),
        );
    }

    out
}

fn message_row(message: &Message, indent: u16, tier: ColorTier) -> Row {
    let (word, bind) = message_face(message.state);
    let sender = message.sender.as_deref().unwrap_or("?");
    let recipient = message.recipient.as_deref().unwrap_or("?");
    let kind = message.kind.as_deref().unwrap_or("message");
    let mut text = format!("{sender} -> {recipient}  {kind}  {word}");
    if let Some(reason) = &message.dead_letter_reason {
        text.push_str(&format!(" -- {reason}"));
    }
    Row {
        indent,
        mark: Some((graph_mark("message"), theme::state_style(bind, tier))),
        text,
        // The stamp is the last state change — for a freshly delivered
        // message, the delivery itself.
        right: Some(hhmm(&message.updated_at).to_string()),
        style: theme::state_style(bind, tier),
        target: Some(BoardTarget::Message(message.id.clone())),
        diagnostic: false,
    }
}

fn event_rows(state: &BoardState, tier: ColorTier) -> Vec<Row> {
    let muted = theme::state_style(theme::binding("idle"), tier);
    let (mut events, duplicate_events) =
        unique_by_id(&state.events, |event| event.event_id.as_str());
    events.retain(|event| {
        state
            .event_tail
            .cursor
            .is_none_or(|cursor| event.global_sequence > cursor)
            && state
                .event_tail
                .aggregate_type
                .as_deref()
                .is_none_or(|kind| event.aggregate_type == kind)
            && state
                .event_tail
                .event_type
                .as_deref()
                .is_none_or(|kind| event.event_type == kind)
    });
    events.sort_by(|left, right| {
        right
            .global_sequence
            .cmp(&left.global_sequence)
            .then_with(|| left.event_id.as_str().cmp(right.event_id.as_str()))
    });

    let findings = duplicate_finding(duplicate_events, "event")
        .into_iter()
        .collect();
    let unknowns = (state.event_tail.dropped > 0)
        .then(|| UnknownNote {
            subject: "tail buffer",
            why: format!(
                "{} removed from memory -- resume from a visible sequence",
                plural(state.event_tail.dropped, "older event", "older events")
            ),
        })
        .into_iter()
        .collect();
    let mut out = pinned_block(findings, unknowns, muted);
    let cursor = state
        .event_tail
        .cursor
        .map_or_else(|| "beginning".to_owned(), |cursor| cursor.to_string());
    let aggregate = state.event_tail.aggregate_type.as_deref().unwrap_or("*");
    let event_type = state.event_tail.event_type.as_deref().unwrap_or("*");
    out.push(Row::plain(
        format!(
            "{}  {} after {cursor}  filter aggregate={aggregate} event={event_type}",
            plural(events.len(), "event", "events"),
            if state.event_tail.live {
                "live"
            } else {
                "page"
            },
        ),
        muted,
    ));
    if events.is_empty() {
        out.push(Row::plain(
            if state.event_tail.live {
                "no matching events delivered -- tail is waiting".to_owned()
            } else {
                "no matching events on this page".to_owned()
            },
            muted,
        ));
        return out;
    }
    out.extend(events.into_iter().map(|event| Row {
        indent: INDENT_STEP,
        mark: None,
        text: format!(
            "#{}  {}  {}  {}/{}",
            event.global_sequence,
            hhmm(&event.appended_at),
            event.event_type,
            event.aggregate_type,
            event.aggregate_id.as_str(),
        ),
        right: Some(event.event_id.as_str().to_owned()),
        style: Style::default(),
        target: Some(BoardTarget::Event(event.event_id.clone())),
        diagnostic: false,
    }));
    out
}

fn replay_seq(frame: &ReplayFrame) -> u64 {
    match frame {
        ReplayFrame::Output { seq, .. } | ReplayFrame::Resize { seq, .. } => *seq,
    }
}

fn replay_elapsed_ms(frame: &ReplayFrame) -> u64 {
    match frame {
        ReplayFrame::Output { elapsed_ms, .. } | ReplayFrame::Resize { elapsed_ms, .. } => {
            *elapsed_ms
        }
    }
}

fn output_preview(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return "(empty)".to_owned();
    }
    let visible = bytes.len().min(OUTPUT_PREVIEW_BYTES);
    let mut preview = String::from_utf8_lossy(&bytes[..visible]).into_owned();
    if visible < bytes.len() {
        preview.push_str(&format!("... (+{} bytes)", bytes.len() - visible));
    }
    preview
}

fn replay_rows(state: &BoardState, tier: ColorTier) -> Vec<Row> {
    let frames = state.replay.frames();
    let muted = theme::state_style(theme::binding("idle"), tier);
    if frames.is_empty() {
        return vec![Row::plain(
            "no replay frames -- nothing recorded".to_owned(),
            muted,
        )];
    }

    let span = frames.last().map(replay_elapsed_ms).unwrap_or(0);
    let mut out = vec![Row::plain(
        format!("{} replay frames  {span}ms span", frames.len()),
        muted,
    )];
    out.extend(frames.iter().map(|frame| {
        let seq = replay_seq(frame);
        let elapsed_ms = replay_elapsed_ms(frame);
        let text = match frame {
            ReplayFrame::Output { bytes, .. } => format!(
                "{elapsed_ms}ms  output  {} bytes  {}",
                bytes.len(),
                output_preview(bytes)
            ),
            ReplayFrame::Resize { cols, rows, .. } => {
                format!("{elapsed_ms}ms  resize  {cols}x{rows}")
            }
        };
        Row {
            indent: 0,
            mark: None,
            text,
            right: Some(format!("seq {seq}")),
            style: Style::default(),
            target: Some(BoardTarget::ReplayFrame(seq)),
            diagnostic: false,
        }
    }));
    out
}

/// One fact the log does not carry, and why it does not.
///
/// The fleet and cost/health panels end their pinned block with these
/// instead of leaving a value blank. A blank cell and a zero look identical
/// on a terminal and at most one of them is ever true, so neither is drawn
/// where the honest answer is "the log does not say".
struct UnknownNote {
    subject: &'static str,
    why: String,
}

/// The pinned block both new panels open with: the page's integrity findings
/// first, then what the log does not carry. Pinned rather than appended,
/// because a panel whose unknowns scrolled off the bottom would read as a
/// panel with none.
fn pinned_block(findings: Vec<String>, unknowns: Vec<UnknownNote>, muted: Style) -> Vec<Row> {
    let mut out = Vec::new();
    if !findings.is_empty() {
        out.push(Row::diagnostic(
            format!(
                "invalid page -- {}",
                plural(findings.len(), "finding", "findings")
            ),
            muted,
        ));
        out.extend(
            findings
                .into_iter()
                .map(|finding| Row::diagnostic(finding, muted)),
        );
    }
    if !unknowns.is_empty() {
        out.push(Row::diagnostic(
            format!(
                "unknown -- {} not in the log",
                plural(unknowns.len(), "fact", "facts")
            ),
            muted,
        ));
        out.extend(
            unknowns
                .into_iter()
                .map(|note| Row::diagnostic(format!("  {}: {}", note.subject, note.why), muted)),
        );
    }
    out
}

/// `1 entry` / `2 entries`. These panels count small sets constantly — a
/// single cost row and a single held lease are the common case, not the
/// edge — so a bare `1 entries` would be the most frequent thing on the
/// frame rather than a rare wart.
fn plural(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// The duplicate-id finding line every panel words the same way.
fn duplicate_finding(count: usize, what: &str) -> Option<String> {
    (count > 0).then(|| {
        let noun = if count == 1 { "id" } else { "ids" };
        format!("+{count} duplicate {what} {noun} -- invalid page")
    })
}

/// `total` against a read that reached the end of every projection, `at
/// least` against one that stopped short. The lens cannot tell the two apart
/// from the rows, so the caller says which and this picks the word.
const fn fold_word(complete: bool) -> &'static str {
    if complete { "total" } else { "at least" }
}

const SUMMARY_ROWS: usize = 5;

fn summary_findings(state: &BoardState, include_cost: bool) -> Vec<String> {
    let (_, duplicate_tasks) = unique_by_id(&state.tasks, |item| item.id.as_str());
    let (_, duplicate_attempts) = unique_by_id(&state.attempts, |item| item.id.as_str());
    let (_, duplicate_attention) = unique_by_id(&state.attention, |item| item.id.as_str());
    let (_, duplicate_worktrees) = unique_by_id(&state.worktrees, |item| item.id.as_str());
    let (_, duplicate_leases) = unique_by_id(&state.leases, |item| item.id.as_str());
    let (_, duplicate_costs) = unique_by_id(&state.costs, |item| item.id.as_str());
    [
        duplicate_finding(duplicate_tasks, "task"),
        duplicate_finding(duplicate_attempts, "attempt"),
        duplicate_finding(duplicate_attention, "attention item"),
        duplicate_finding(duplicate_worktrees, "worktree"),
        duplicate_finding(duplicate_leases, "lease"),
        include_cost
            .then(|| duplicate_finding(duplicate_costs, "cost entry"))
            .flatten(),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn ranked_attention(items: &[AttentionItem]) -> Vec<&AttentionItem> {
    let (mut open, _) = unique_by_id(items, |item| item.id.as_str());
    open.retain(|item| item.resolved_at.is_none());
    open.sort_by(|left, right| {
        match (left.priority, right.priority) {
            (Some(a), Some(b)) => a.cmp(&b),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then_with(|| left.id.as_str().cmp(right.id.as_str()))
    });
    open
}

fn recent_activity(state: &BoardState) -> Vec<ActivityFact> {
    let (tasks, _) = unique_by_id(&state.tasks, |item| item.id.as_str());
    let (attempts, _) = unique_by_id(&state.attempts, |item| item.id.as_str());
    let (attention, _) = unique_by_id(&state.attention, |item| item.id.as_str());
    let (worktrees, _) = unique_by_id(&state.worktrees, |item| item.id.as_str());
    let (leases, _) = unique_by_id(&state.leases, |item| item.id.as_str());
    let mut facts = Vec::new();
    facts.extend(tasks.into_iter().map(|task| ActivityFact {
        at: task.updated_at.clone(),
        kind: "task".to_owned(),
        id: task.id.as_str().to_owned(),
        summary: format!(
            "{}  {}",
            task.title.as_deref().unwrap_or("untitled"),
            task_face(task.state).0
        ),
    }));
    facts.extend(attempts.into_iter().map(|attempt| ActivityFact {
        at: attempt.updated_at.clone(),
        kind: "attempt".to_owned(),
        id: attempt.id.as_str().to_owned(),
        summary: format!(
            "{}  {}",
            attempt.engine.as_str(),
            attempt_face(attempt.state).0
        ),
    }));
    facts.extend(attention.into_iter().map(|item| ActivityFact {
        at: item.resolved_at.as_ref().unwrap_or(&item.raised_at).clone(),
        kind: "attention_item".to_owned(),
        id: item.id.as_str().to_owned(),
        summary: format!(
            "{}  {}",
            if item.resolved_at.is_some() {
                "resolved"
            } else {
                "raised"
            },
            item.summary
        ),
    }));
    facts.extend(worktrees.into_iter().map(|worktree| {
        ActivityFact {
            at: worktree
                .released_at
                .as_ref()
                .unwrap_or(&worktree.created_at)
                .clone(),
            kind: "worktree".to_owned(),
            id: worktree.id.as_str().to_owned(),
            summary: format!(
                "{}  {}  {}",
                worktree.repo,
                worktree.branch,
                worktree_face(worktree).0
            ),
        }
    }));
    facts.extend(leases.into_iter().map(|lease| ActivityFact {
        at: lease.updated_at.clone(),
        kind: "lease".to_owned(),
        id: lease.id.as_str().to_owned(),
        summary: format!(
            "{}  {}",
            lease.scope.as_deref().unwrap_or("unnamed scope"),
            lease_face(lease.state).0
        ),
    }));
    facts.sort_by(|left, right| {
        right
            .at
            .as_str()
            .cmp(left.at.as_str())
            .then_with(|| left.kind.cmp(&right.kind))
            .then_with(|| left.id.cmp(&right.id))
    });
    facts.truncate(SUMMARY_ROWS);
    facts
}

fn cost_headline(costs: &[CostEntry]) -> CostHeadline {
    let (costs, _) = unique_by_id(costs, |item| item.id.as_str());
    let mut micros = 0u128;
    let mut priced = 0usize;
    let mut estimated = 0usize;
    for cost in &costs {
        if let Some(value) = cost.cost_micros {
            micros = micros.saturating_add(u128::from(value.value()));
            priced += 1;
        }
        estimated += usize::from(cost.cost_is_estimate == Some(true));
    }
    CostHeadline {
        entries: costs.len(),
        priced_entries: priced,
        unpriced_entries: costs.len().saturating_sub(priced),
        estimated_entries: estimated,
        cost_micros: (priced > 0).then(|| micros.to_string()),
    }
}

/// Build the budget value shared by attempt detail and `gw attempt budget`.
pub fn attempt_budget(attempt: &Attempt) -> AttemptBudget {
    let budget = attempt.budget.as_ref();
    let mut uncapped_axes = Vec::new();
    if budget.is_none_or(|value| value.max_tokens.is_none()) {
        uncapped_axes.push("max_tokens");
    }
    if budget.is_none_or(|value| value.max_tool_calls.is_none()) {
        uncapped_axes.push("max_tool_calls");
    }
    if budget.is_none_or(|value| value.max_wall_ms.is_none()) {
        uncapped_axes.push("max_wall_ms");
    }
    if budget.is_none_or(|value| value.max_cost_micros.is_none()) {
        uncapped_axes.push("max_cost_micros");
    }
    AttemptBudget {
        kind: "attempt_budget",
        attempt_id: attempt.id.clone(),
        version: attempt.version,
        budget: attempt.budget.clone(),
        uncapped_axes,
    }
}

/// Build the shared estate summary consumed by the Board and `gw estate overview`.
pub fn estate_overview(state: &BoardState) -> EstateOverview {
    let (tasks, _) = unique_by_id(&state.tasks, |item| item.id.as_str());
    let (attempts, _) = unique_by_id(&state.attempts, |item| item.id.as_str());
    let attention = ranked_attention(&state.attention);
    let (worktrees, _) = unique_by_id(&state.worktrees, |item| item.id.as_str());
    let (leases, _) = unique_by_id(&state.leases, |item| item.id.as_str());
    EstateOverview {
        kind: "estate_overview",
        complete: state.complete,
        watermark: state.watermark,
        counts: EstateCounts {
            tasks: tasks.len(),
            active_tasks: tasks
                .iter()
                .filter(|task| {
                    matches!(
                        task.state,
                        TaskState::Submitted | TaskState::Working | TaskState::InputRequired
                    )
                })
                .count(),
            attempts: attempts.len(),
            running_attempts: attempts
                .iter()
                .filter(|attempt| attempt.state == AttemptState::Running)
                .count(),
            unresolved_attention: attention.len(),
            held_worktrees: worktrees
                .iter()
                .filter(|worktree| worktree.released_at.is_none())
                .count(),
            held_leases: leases
                .iter()
                .filter(|lease| lease.state == LeaseState::Held)
                .count(),
        },
        attention_head: attention.first().map(|item| AttentionHead {
            id: item.id.clone(),
            summary: item.summary.clone(),
            priority: item.priority,
            subject_ref: item.subject_ref.clone(),
            raised_at: item.raised_at.clone(),
            acked_at: item.acked_at.clone(),
            muted_until: item.muted_until.clone(),
        }),
        recent_activity: recent_activity(state),
        findings: summary_findings(state, false),
        unknowns: (!state.complete)
            .then(|| "read short of the last projection page -- counts are floors".to_owned())
            .into_iter()
            .collect(),
    }
}

/// Build the shared activity digest consumed by the Board and `gw activity brief`.
pub fn activity_brief(state: &BoardState) -> ActivityBrief {
    let mut owed = Vec::new();
    owed.extend(
        ranked_attention(&state.attention)
            .into_iter()
            .map(|item| OwedFact {
                kind: "attention_item".to_owned(),
                id: item.id.as_str().to_owned(),
                reason: item.summary.clone(),
            }),
    );
    let (tasks, _) = unique_by_id(&state.tasks, |item| item.id.as_str());
    owed.extend(
        tasks
            .into_iter()
            .filter(|task| task.state == TaskState::InputRequired)
            .map(|task| OwedFact {
                kind: "task".to_owned(),
                id: task.id.as_str().to_owned(),
                reason: "input required".to_owned(),
            }),
    );
    let (attempts, _) = unique_by_id(&state.attempts, |item| item.id.as_str());
    owed.extend(
        attempts
            .into_iter()
            .filter(|attempt| attempt.state == AttemptState::Blocked)
            .map(|attempt| OwedFact {
                kind: "attempt".to_owned(),
                id: attempt.id.as_str().to_owned(),
                reason: "blocked".to_owned(),
            }),
    );
    let (worktrees, _) = unique_by_id(&state.worktrees, |item| item.id.as_str());
    owed.extend(
        worktrees
            .into_iter()
            .filter(|worktree| {
                worktree.released_at.is_none() && (worktree.dirty || worktree.unpushed)
            })
            .map(|worktree| OwedFact {
                kind: "worktree".to_owned(),
                id: worktree.id.as_str().to_owned(),
                reason: match (worktree.dirty, worktree.unpushed) {
                    (true, true) => "held, dirty, and unpushed",
                    (true, false) => "held and dirty",
                    (false, true) => "held and unpushed",
                    (false, false) => "held",
                }
                .to_owned(),
            }),
    );
    owed.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.id.cmp(&right.id))
    });
    let owed_total = owed.len();
    owed.truncate(SUMMARY_ROWS);
    ActivityBrief {
        kind: "activity_brief",
        complete: state.complete,
        watermark: state.watermark,
        happened: recent_activity(state),
        owed_total,
        owed,
        cost: cost_headline(&state.costs),
        findings: summary_findings(state, true),
        unknowns: (!state.complete)
            .then(|| {
                "read short of the last projection page -- activity and debt are page-scoped"
                    .to_owned()
            })
            .into_iter()
            .collect(),
    }
}

fn fact_target(fact: &ActivityFact) -> Option<BoardTarget> {
    match fact.kind.as_str() {
        "task" => Some(BoardTarget::Task(TaskId::new(&fact.id))),
        "attempt" => Some(BoardTarget::Attempt(AttemptId::new(&fact.id))),
        "attention_item" => Some(BoardTarget::Attention(AttentionItemId::new(&fact.id))),
        "worktree" => Some(BoardTarget::Worktree(WorktreeId::new(&fact.id))),
        "lease" => Some(BoardTarget::Lease(LeaseId::new(&fact.id))),
        _ => None,
    }
}

fn owed_target(fact: &OwedFact) -> Option<BoardTarget> {
    match fact.kind.as_str() {
        "task" => Some(BoardTarget::Task(TaskId::new(&fact.id))),
        "attempt" => Some(BoardTarget::Attempt(AttemptId::new(&fact.id))),
        "attention_item" => Some(BoardTarget::Attention(AttentionItemId::new(&fact.id))),
        "worktree" => Some(BoardTarget::Worktree(WorktreeId::new(&fact.id))),
        _ => None,
    }
}

fn estate_rows(state: &BoardState, tier: ColorTier) -> Vec<Row> {
    let muted = theme::state_style(theme::binding("idle"), tier);
    let summary = estate_overview(state);
    let unknowns = summary
        .unknowns
        .iter()
        .map(|why| UnknownNote {
            subject: "snapshot",
            why: why.clone(),
        })
        .collect();
    let mut out = pinned_block(summary.findings.clone(), unknowns, muted);
    out.push(Row::plain(
        format!(
            "{}  {} active  {}  {} running",
            plural(summary.counts.tasks, "task", "tasks"),
            summary.counts.active_tasks,
            plural(summary.counts.attempts, "attempt", "attempts"),
            summary.counts.running_attempts,
        ),
        muted,
    ));
    out.push(Row::plain(
        format!(
            "{}  {} held  {} held",
            plural(
                summary.counts.unresolved_attention,
                "unresolved attention item",
                "unresolved attention items"
            ),
            plural(summary.counts.held_worktrees, "worktree", "worktrees"),
            plural(summary.counts.held_leases, "lease", "leases"),
        ),
        muted,
    ));
    out.push(Row::plain("attention head".to_owned(), muted));
    if let Some(head) = summary.attention_head {
        out.push(Row {
            indent: INDENT_STEP,
            mark: None,
            text: head.summary,
            right: Some(head.id.as_str().to_owned()),
            style: theme::state_style(theme::binding("needs_attention"), tier),
            target: Some(BoardTarget::Attention(head.id)),
            diagnostic: false,
        });
    } else {
        out.push(Row::plain(
            "  none unresolved on this page".to_owned(),
            muted,
        ));
    }
    out.push(Row::plain("recent activity".to_owned(), muted));
    if summary.recent_activity.is_empty() {
        out.push(Row::plain(
            "  no projection activity on this page".to_owned(),
            muted,
        ));
    } else {
        out.extend(summary.recent_activity.into_iter().map(|fact| {
            let target = fact_target(&fact);
            Row {
                indent: INDENT_STEP,
                mark: None,
                text: format!("{}  {}", fact.kind, fact.summary),
                right: Some(hhmm(&fact.at).to_owned()),
                style: Style::default(),
                target,
                diagnostic: false,
            }
        }));
    }
    out
}

fn activity_rows(state: &BoardState, tier: ColorTier) -> Vec<Row> {
    let muted = theme::state_style(theme::binding("idle"), tier);
    let summary = activity_brief(state);
    let unknowns = summary
        .unknowns
        .iter()
        .map(|why| UnknownNote {
            subject: "snapshot",
            why: why.clone(),
        })
        .collect();
    let mut out = pinned_block(summary.findings.clone(), unknowns, muted);
    out.push(Row::plain("what happened".to_owned(), muted));
    if summary.happened.is_empty() {
        out.push(Row::plain(
            "  no recorded changes on this page".to_owned(),
            muted,
        ));
    } else {
        out.extend(summary.happened.into_iter().map(|fact| {
            let target = fact_target(&fact);
            Row {
                indent: INDENT_STEP,
                mark: None,
                text: format!("{}  {}", fact.kind, fact.summary),
                right: Some(hhmm(&fact.at).to_owned()),
                style: Style::default(),
                target,
                diagnostic: false,
            }
        }));
    }
    out.push(Row::plain(
        format!(
            "what is owed  {}",
            plural(summary.owed_total, "fact", "facts")
        ),
        muted,
    ));
    if summary.owed.is_empty() {
        out.push(Row::plain(
            "  nothing owed by these projections".to_owned(),
            muted,
        ));
    } else {
        out.extend(summary.owed.into_iter().map(|fact| {
            let target = owed_target(&fact);
            Row {
                indent: INDENT_STEP,
                mark: None,
                text: format!("{} {}  {}", fact.kind, fact.id, fact.reason),
                right: None,
                style: theme::state_style(theme::binding("needs_attention"), tier),
                target,
                diagnostic: false,
            }
        }));
    }
    out.push(Row::plain(
        if summary.cost.entries == 0 {
            "cost  no entries -- no spend recorded on this page".to_owned()
        } else {
            format!(
                "cost  {}  {} priced  {} unpriced  {}",
                plural(summary.cost.entries, "entry", "entries"),
                summary.cost.priced_entries,
                summary.cost.unpriced_entries,
                summary.cost.cost_micros.map_or_else(
                    || "no cost reported".to_owned(),
                    |value| format!("{value} micros")
                ),
            )
        },
        muted,
    ));
    out
}

fn session_face(session: &EngineSession) -> (&'static str, &'static StateBinding) {
    match session.ended_at {
        Some(_) => ("ended", theme::binding("done")),
        // The log holds no end stamp. That is not the claim that the session
        // is alive: nothing here heartbeats, probes, or watches a process,
        // so `unended` is the whole of what can honestly be said.
        None => ("no end recorded", theme::binding("unknown")),
    }
}

fn worktree_face(worktree: &Worktree) -> (&'static str, &'static StateBinding) {
    match worktree.released_at {
        Some(_) => ("released", theme::binding("done")),
        None => ("held", theme::binding("running")),
    }
}

fn lease_face(state: LeaseState) -> (&'static str, &'static StateBinding) {
    match state {
        LeaseState::Held => ("held", theme::binding("running")),
        LeaseState::Released => ("released", theme::binding("done")),
        // A lapse, not a failure — the warn token says "look", not "it broke".
        LeaseState::Expired => ("expired", theme::binding("unknown")),
    }
}

/// The running estate: engine sessions, the worktrees and leases they hold,
/// and the attempt/spawn counts the DAG panel details.
pub fn agent_fleet(state: &BoardState) -> AgentFleet {
    let (sessions, duplicate_sessions) =
        unique_by_id(&state.sessions, |session| session.id.as_str());
    let (worktrees, duplicate_worktrees) =
        unique_by_id(&state.worktrees, |worktree| worktree.id.as_str());
    let (leases, duplicate_leases) = unique_by_id(&state.leases, |lease| lease.id.as_str());
    let (attempts, _) = unique_by_id(&state.attempts, |attempt| attempt.id.as_str());
    let (nodes, _) = unique_by_id(&state.nodes, |node| node.id.as_str());

    let findings = [
        duplicate_finding(duplicate_sessions, "session"),
        duplicate_finding(duplicate_worktrees, "worktree"),
        duplicate_finding(duplicate_leases, "lease"),
    ]
    .into_iter()
    .flatten()
    .collect();

    let unended = sessions
        .iter()
        .filter(|session| session.ended_at.is_none())
        .count();
    let held_worktrees = worktrees
        .iter()
        .filter(|worktree| worktree.released_at.is_none())
        .count();
    let held_leases = leases
        .iter()
        .filter(|lease| lease.state == LeaseState::Held)
        .count();
    let attempts_without_session = {
        let bound: std::collections::BTreeSet<&str> = sessions
            .iter()
            .map(|session| session.attempt_id.as_str())
            .collect();
        attempts
            .iter()
            .filter(|attempt| attempt.state == AttemptState::Running)
            .filter(|attempt| !bound.contains(attempt.id.as_str()))
            .count()
    };

    let mut unknowns: Vec<FleetUnknown> = Vec::new();
    if unended > 0 {
        unknowns.push(FleetUnknown {
            subject: "liveness",
            why: format!(
                "no end stamp on {unended} of {} -- unended is not alive",
                plural(sessions.len(), "session", "sessions"),
            ),
        });
    }
    if attempts_without_session > 0 {
        unknowns.push(FleetUnknown {
            subject: "engine binding",
            why: format!(
                "none on this page for {}",
                plural(
                    attempts_without_session,
                    "running attempt",
                    "running attempts"
                ),
            ),
        });
    }
    if !state.complete {
        unknowns.push(FleetUnknown {
            subject: "fleet size",
            why: "read short of the last page -- counts are floors".to_owned(),
        });
    }

    AgentFleet {
        kind: "agent_fleet",
        complete: state.complete,
        watermark: state.watermark,
        counts: FleetCounts {
            sessions: sessions.len(),
            unended_sessions: unended,
            running_attempts: attempts
                .iter()
                .filter(|attempt| attempt.state == AttemptState::Running)
                .count(),
            attempts_without_session,
            spawns: nodes.len(),
            held_worktrees,
            held_leases,
        },
        sessions: sessions.into_iter().cloned().collect(),
        dispatch_nodes: nodes.into_iter().cloned().collect(),
        worktrees: worktrees.into_iter().cloned().collect(),
        leases: leases.into_iter().cloned().collect(),
        findings,
        unknowns,
    }
}

fn fleet_rows(state: &BoardState, tier: ColorTier) -> Vec<Row> {
    let muted = theme::state_style(theme::binding("idle"), tier);
    let summary = agent_fleet(state);
    let unknowns = summary
        .unknowns
        .iter()
        .map(|unknown| UnknownNote {
            subject: unknown.subject,
            why: unknown.why.clone(),
        })
        .collect();

    let mut out = pinned_block(summary.findings.clone(), unknowns, muted);
    out.push(Row::plain(
        format!(
            "{}  {} running  {} held  {} held  {}",
            plural(summary.counts.sessions, "session", "sessions"),
            plural(summary.counts.running_attempts, "attempt", "attempts"),
            plural(summary.counts.held_worktrees, "worktree", "worktrees"),
            plural(summary.counts.held_leases, "lease", "leases"),
            plural(summary.counts.spawns, "spawn", "spawns"),
        ),
        muted,
    ));

    if summary.sessions.is_empty() && summary.worktrees.is_empty() && summary.leases.is_empty() {
        out.push(Row::plain(
            "no fleet -- no sessions, worktrees, or leases on this page".to_owned(),
            muted,
        ));
        return out;
    }

    if !summary.sessions.is_empty() {
        out.push(Row::plain("engine sessions".to_owned(), muted));
        out.extend(summary.sessions.iter().map(|session| {
            let (word, bind) = session_face(session);
            Row {
                indent: INDENT_STEP,
                mark: None,
                text: format!(
                    "{}  {word}  started {}  attempt {}",
                    session.engine.as_str(),
                    hhmm(&session.started_at),
                    session.attempt_id.as_str(),
                ),
                right: Some(session.id.as_str().to_owned()),
                style: theme::state_style(bind, tier),
                target: Some(BoardTarget::Session(session.id.clone())),
                diagnostic: false,
            }
        }));
    }

    if !summary.worktrees.is_empty() {
        out.push(Row::plain("worktrees".to_owned(), muted));
        out.extend(summary.worktrees.iter().map(|worktree| {
            let (word, bind) = worktree_face(worktree);
            let mut text = format!("{}  {}  {word}", worktree.repo, worktree.branch);
            if worktree.dirty {
                text.push_str("  dirty");
            }
            if worktree.unpushed {
                text.push_str("  unpushed");
            }
            Row {
                indent: INDENT_STEP,
                mark: None,
                text,
                right: Some(worktree.id.as_str().to_owned()),
                style: theme::state_style(bind, tier),
                target: Some(BoardTarget::Worktree(worktree.id.clone())),
                diagnostic: false,
            }
        }));
    }

    if !summary.leases.is_empty() {
        out.push(Row::plain("leases".to_owned(), muted));
        out.extend(summary.leases.iter().map(|lease| {
            let (word, bind) = lease_face(lease.state);
            Row {
                indent: INDENT_STEP,
                mark: None,
                text: format!(
                    "{}  {word}  holder {}  scope {}",
                    match lease.mode {
                        gwk_domain::fsm::LeaseMode::Exclusive => "exclusive",
                        gwk_domain::fsm::LeaseMode::Shared => "shared",
                    },
                    lease.holder.as_deref().unwrap_or("unnamed"),
                    lease.scope.as_deref().unwrap_or("unnamed"),
                ),
                right: Some(lease.id.as_str().to_owned()),
                style: theme::state_style(bind, tier),
                target: Some(BoardTarget::Lease(lease.id.clone())),
                diagnostic: false,
            }
        }));
    }

    out
}

/// Micro-USD at the ledger's own resolution. Six decimals and no rounding:
/// most token rows are sub-cent, and two decimals would print them as zero —
/// which is the one value the ledger is careful never to invent.
fn usd(micros: u128) -> String {
    format!("{}.{:06} USD", micros / 1_000_000, micros % 1_000_000)
}

/// One token column's fold: the sum, and how many rows reported it at all.
/// A column no row reports is `not reported` and never `0`, because zero is
/// the claim that an engine used none of it.
#[derive(Debug, Default, Clone, Copy)]
struct TokenFold {
    sum: u128,
    rows: usize,
}

impl TokenFold {
    fn add(&mut self, value: Option<gwk_domain::ids::TokenCount>) {
        if let Some(count) = value {
            self.sum = self.sum.saturating_add(u128::from(count.value()));
            self.rows += 1;
        }
    }

    fn coverage(self) -> TokenCoverage {
        TokenCoverage {
            total: (self.rows > 0).then(|| self.sum.to_string()),
            reported_entries: self.rows,
        }
    }
}

impl TokenCoverage {
    fn line(&self, label: &str, entries: usize, complete: bool) -> String {
        let Some(total) = self.total.as_deref() else {
            return format!("{label} not reported");
        };
        format!(
            "{label} {} {} over {} of {}",
            total,
            fold_word(complete),
            self.reported_entries,
            plural(entries, "entry", "entries"),
        )
    }
}

/// Fold exactly the cost rows supplied by the caller. A short read remains a
/// floor and an empty read remains an explicit statement, never a zero-cost
/// claim inferred from absence.
pub fn cost_rollup(state: &BoardState) -> CostRollup {
    let (costs, duplicate_costs) = unique_by_id(&state.costs, |cost| cost.id.as_str());
    let mut input = TokenFold::default();
    let mut cached_input = TokenFold::default();
    let mut cache_write = TokenFold::default();
    let mut output = TokenFold::default();
    let mut reasoning = TokenFold::default();
    let mut buckets: std::collections::BTreeMap<(String, Option<String>), (usize, u128, usize)> =
        std::collections::BTreeMap::new();

    for cost in &costs {
        input.add(cost.input_tokens);
        cached_input.add(cost.cached_input_tokens);
        cache_write.add(cost.cache_write_tokens);
        output.add(cost.output_tokens);
        reasoning.add(cost.reasoning_tokens);

        let bucket = buckets
            .entry((cost.engine.as_str().to_owned(), cost.model.clone()))
            .or_default();
        bucket.0 += 1;
        if let Some(value) = cost.cost_micros {
            bucket.1 = bucket.1.saturating_add(u128::from(value.value()));
            bucket.2 += 1;
        }
    }

    let headline = cost_headline(&state.costs);
    let mut entries: Vec<CostEntry> = costs.iter().map(|cost| (*cost).clone()).collect();
    entries.sort_by(|left, right| {
        right
            .recorded_at
            .as_str()
            .cmp(left.recorded_at.as_str())
            .then_with(|| left.id.as_str().cmp(right.id.as_str()))
    });

    let mut unknowns = Vec::new();
    if headline.entries == 0 {
        unknowns.push(CostUnknown {
            subject: "cost",
            why: "no entries -- no spend recorded on this page".to_owned(),
        });
    }
    if headline.unpriced_entries > 0 {
        unknowns.push(CostUnknown {
            subject: "currency",
            why: format!(
                "tokens only on {} of {} -- the ledger never converts",
                headline.unpriced_entries,
                plural(headline.entries, "entry", "entries"),
            ),
        });
    }
    if !state.complete {
        unknowns.push(CostUnknown {
            subject: "the ledger",
            why: "read short of the last page -- figures are floors".to_owned(),
        });
    }

    CostRollup {
        kind: "cost_rollup",
        complete: state.complete,
        watermark: state.watermark,
        headline,
        by_engine: buckets
            .into_iter()
            .map(
                |((engine, model), (entries, micros, priced_entries))| EngineCostRollup {
                    engine,
                    model,
                    entries,
                    priced_entries,
                    cost_micros: (priced_entries > 0).then(|| micros.to_string()),
                },
            )
            .collect(),
        tokens: CostTokens {
            input: input.coverage(),
            output: output.coverage(),
            cached_input: cached_input.coverage(),
            cache_write: cache_write.coverage(),
            reasoning: reasoning.coverage(),
        },
        entries,
        findings: duplicate_finding(duplicate_costs, "cost entry")
            .into_iter()
            .collect(),
        unknowns,
    }
}

/// Burn and liveness: the `cost_entry` ledger folded client-side, and the
/// `health`/`session` ingestion kinds counted.
fn cost_health_rows(state: &BoardState, tier: ColorTier) -> Vec<Row> {
    let muted = theme::state_style(theme::binding("idle"), tier);

    let summary = cost_rollup(state);
    let (ingested, duplicate_ingested) = unique_by_id(&state.ingested, |record| record.id.as_str());

    let mut findings = summary.findings.clone();
    if let Some(finding) = duplicate_finding(duplicate_ingested, "ingested record") {
        findings.push(finding);
    }

    let health = ingested
        .iter()
        .filter(|record| record.kind == IngestionKind::Health)
        .count();
    let session_records: Vec<&&IngestedRecord> = ingested
        .iter()
        .filter(|record| record.kind == IngestionKind::Session)
        .collect();

    let mut unknowns: Vec<UnknownNote> = summary
        .unknowns
        .iter()
        // The headline below is the cost panel's presentation of this same
        // typed absence; pinning it as well would state one fact twice.
        .filter(|note| note.subject != "cost")
        .map(|note| UnknownNote {
            subject: note.subject,
            why: note.why.clone(),
        })
        .collect();
    if health == 0 {
        unknowns.push(UnknownNote {
            subject: "health",
            // "no records" is a claim about the whole ledger and this count is
            // over one page, so a partial read says so — the same distinction
            // the floor note below draws, applied where it is also true.
            why: if state.complete {
                "no records -- ingestion is operator-driven, no producer".to_owned()
            } else {
                "none on this page -- ingestion is operator-driven, no producer".to_owned()
            },
        });
    }
    let mut out = pinned_block(findings, unknowns, muted);
    out.push(Row::plain(
        if summary.headline.entries == 0 {
            "cost  no entries -- no spend recorded on this page".to_owned()
        } else {
            let currency = summary.headline.cost_micros.as_deref().map_or_else(
                || "no cost reported".to_owned(),
                |micros| usd(micros.parse().expect("the cost fold emits decimal micros")),
            );
            format!(
                "cost  {}  {} {} over {} of {} priced  {} estimated",
                plural(summary.headline.entries, "entry", "entries"),
                currency,
                fold_word(state.complete),
                summary.headline.priced_entries,
                summary.headline.entries,
                summary.headline.estimated_entries,
            )
        },
        muted,
    ));

    if summary.headline.entries > 0 {
        out.push(Row::plain("by engine".to_owned(), muted));
        out.extend(summary.by_engine.iter().map(|bucket| Row {
            indent: INDENT_STEP,
            mark: None,
            text: format!(
                "{}  {}  {}",
                bucket.engine,
                bucket.model.as_deref().unwrap_or("model unreported"),
                plural(bucket.entries, "entry", "entries")
            ),
            right: Some(bucket.cost_micros.as_deref().map_or_else(
                || "no cost reported".to_owned(),
                |micros| usd(micros.parse().expect("the cost fold emits decimal micros")),
            )),
            style: Style::default(),
            target: None,
            diagnostic: false,
        }));

        out.push(Row::plain("tokens".to_owned(), muted));
        for line in [
            summary
                .tokens
                .input
                .line("input", summary.headline.entries, state.complete),
            summary
                .tokens
                .output
                .line("output", summary.headline.entries, state.complete),
            summary.tokens.cached_input.line(
                "cached input",
                summary.headline.entries,
                state.complete,
            ),
            summary.tokens.cache_write.line(
                "cache write",
                summary.headline.entries,
                state.complete,
            ),
            summary
                .tokens
                .reasoning
                .line("reasoning", summary.headline.entries, state.complete),
        ] {
            out.push(Row {
                indent: INDENT_STEP,
                mark: None,
                text: line,
                right: None,
                style: Style::default(),
                target: None,
                diagnostic: false,
            });
        }

        out.push(Row::plain("entries".to_owned(), muted));
        out.extend(summary.entries.iter().map(|cost| Row {
            indent: INDENT_STEP,
            mark: None,
            text: format!(
                "{}  {}  {}",
                hhmm(&cost.recorded_at),
                cost.engine.as_str(),
                match (cost.cost_micros, cost.cost_is_estimate) {
                    (Some(value), Some(true)) =>
                        format!("{} estimated", usd(u128::from(value.value()))),
                    (Some(value), _) => usd(u128::from(value.value())),
                    (None, _) => "no cost reported".to_owned(),
                },
            ),
            right: Some(cost.id.as_str().to_owned()),
            style: Style::default(),
            target: Some(BoardTarget::Cost(cost.id.clone())),
            diagnostic: false,
        }));
    }

    out.push(Row::plain("ingestion".to_owned(), muted));
    out.push(Row {
        indent: INDENT_STEP,
        mark: None,
        text: format!("health {}", plural(health, "record", "records")),
        right: None,
        style: if health == 0 {
            theme::state_style(theme::binding("unknown"), tier)
        } else {
            Style::default()
        },
        target: None,
        diagnostic: false,
    });
    out.push(Row {
        indent: INDENT_STEP,
        mark: None,
        text: format!(
            "session {}{}",
            plural(session_records.len(), "record", "records"),
            session_records
                .iter()
                .map(|record| record.ingested_at.as_str())
                .max()
                .map(|newest| format!("  newest {newest}"))
                .unwrap_or_default(),
        ),
        right: None,
        style: if session_records.is_empty() {
            theme::state_style(theme::binding("unknown"), tier)
        } else {
            Style::default()
        },
        target: None,
        diagnostic: false,
    });
    out.extend(session_records.into_iter().map(|record| Row {
        indent: INDENT_STEP * 2,
        mark: None,
        text: format!("{}  seq {}", record.ingested_at.as_str(), record.event_seq),
        right: Some(record.id.as_str().to_owned()),
        style: Style::default(),
        target: Some(BoardTarget::Ingested(record.id.clone())),
        diagnostic: false,
    }));

    out
}

/// `kind:id` for an actor, or the word for an actor that named no id.
fn actor_face(actor: &gwk_domain::envelope::Actor) -> String {
    match &actor.id {
        Some(id) => format!("{}:{id}", actor.kind),
        None => actor.kind.clone(),
    }
}

/// The attestation ledger: who did what to which subject, and on what basis.
///
/// The kernel mints a receipt in the same transaction as the action it
/// attests, so this is not a log a writer could forget to append to — which
/// is the property that makes it an audit ledger rather than a diary.
fn audit_rows(state: &BoardState, tier: ColorTier) -> Vec<Row> {
    let muted = theme::state_style(theme::binding("idle"), tier);
    let (receipts, duplicate_receipts) =
        unique_by_id(&state.receipts, |receipt| receipt.id.as_str());

    let findings: Vec<String> = duplicate_finding(duplicate_receipts, "receipt")
        .into_iter()
        .collect();

    let basisless = receipts
        .iter()
        .filter(|receipt| receipt.observed_basis.is_none())
        .count();
    let mut unknowns = Vec::new();
    if basisless > 0 {
        unknowns.push(UnknownNote {
            subject: "basis",
            why: format!(
                "no observed basis on {basisless} of {}",
                plural(receipts.len(), "receipt", "receipts"),
            ),
        });
    }
    if !state.complete {
        unknowns.push(UnknownNote {
            subject: "the ledger",
            why: "read short of the last page -- counts are floors".to_owned(),
        });
    }

    let mut out = pinned_block(findings, unknowns, muted);
    if receipts.is_empty() {
        out.push(Row::plain(
            "no receipts -- nothing attested on this page".to_owned(),
            muted,
        ));
        return out;
    }

    let mut by_action: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for receipt in &receipts {
        *by_action.entry(receipt.action.as_str()).or_default() += 1;
    }
    out.push(Row::plain(
        format!(
            "{}  {} across {}",
            plural(receipts.len(), "receipt", "receipts"),
            fold_word(state.complete),
            plural(by_action.len(), "action", "actions"),
        ),
        muted,
    ));

    // Newest first among the rows read. The wire pages by id in byte order
    // and offers no time ordering, so this recency is over the page — the
    // pinned block says so when the page was a prefix.
    let mut recent: Vec<&&Receipt> = receipts.iter().collect();
    recent.sort_by(|left, right| {
        right
            .ts
            .as_str()
            .cmp(left.ts.as_str())
            .then_with(|| left.id.as_str().cmp(right.id.as_str()))
    });
    out.extend(recent.into_iter().map(|receipt| {
        // The edge when the receipt records one, because "queued -> running"
        // is the whole content of a state-flip attestation and printing the
        // action alone would drop it.
        let edge = match (&receipt.from, &receipt.to) {
            (Some(from), Some(to)) => format!("  {from} -> {to}"),
            (None, Some(to)) => format!("  -> {to}"),
            (Some(from), None) => format!("  {from} ->"),
            (None, None) => String::new(),
        };
        Row {
            indent: 0,
            mark: None,
            text: format!(
                "{}  {}  {} {}{edge}",
                hhmm(&receipt.ts),
                receipt.action,
                receipt.subject_type,
                receipt.subject_id,
            ),
            right: Some(actor_face(&receipt.actor)),
            style: Style::default(),
            target: Some(BoardTarget::Receipt(receipt.id.clone())),
            diagnostic: false,
        }
    }));

    out.push(Row::plain("by action".to_owned(), muted));
    out.extend(by_action.into_iter().map(|(action, count)| Row {
        indent: INDENT_STEP,
        mark: None,
        text: action.to_owned(),
        right: Some(plural(count, "receipt", "receipts")),
        style: Style::default(),
        target: None,
        diagnostic: false,
    }));

    out
}

fn rows(state: &BoardState, tier: ColorTier) -> Vec<Row> {
    match state.view {
        BoardView::Estate => estate_rows(state, tier),
        BoardView::Activity => activity_rows(state, tier),
        BoardView::Runs => workflow_run_rows(state, tier),
        BoardView::Dag => dag_rows(state, tier),
        BoardView::Flow => flow_rows(state, tier),
        BoardView::Events => event_rows(state, tier),
        BoardView::Replay => replay_rows(state, tier),
        BoardView::Fleet => fleet_rows(state, tier),
        BoardView::CostHealth => cost_health_rows(state, tier),
        BoardView::Audit => audit_rows(state, tier),
    }
}

/// Every actionable target in deterministic visual order, including rows that
/// the current geometry cannot paint. Keyboard navigation walks this order;
/// the frame then windows around the resulting selection, while [`HitMap`]
/// remains limited to clickable rows that are actually visible.
pub fn target_order(state: &BoardState) -> Vec<BoardTarget> {
    rows(state, ColorTier::Mono)
        .into_iter()
        .filter_map(|row| row.target)
        .collect()
}

fn wrap_detail(lines: &[String], width: usize, limit: usize) -> (Vec<String>, bool) {
    let mut wrapped = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let remaining = limit.saturating_sub(wrapped.len());
        if remaining == 0 {
            return (wrapped, true);
        }
        let (parts, cut) = theme::safe_text_lines(line, width, remaining);
        wrapped.extend(parts);
        if cut || (wrapped.len() == limit && index + 1 < lines.len()) {
            return (wrapped, true);
        }
    }
    (wrapped, false)
}

/// The selection detail pane's lines: the full fields the row truncates.
/// `None` when the selected entity is not in this state — a stale selection
/// gets no pane, not a fabricated one.
fn detail_lines(state: &BoardState, target: &BoardTarget) -> Option<Vec<String>> {
    fn opt(label: &str, value: Option<&str>) -> String {
        value.map(|v| format!("  {label} {v}")).unwrap_or_default()
    }
    match target {
        BoardTarget::Task(id) => {
            let task = state.tasks.iter().find(|t| t.id.as_str() == id.as_str())?;
            let (word, _) = task_face(task.state);
            Some(vec![
                format!("task {}", task.id.as_str()),
                task.title.clone().unwrap_or_else(|| "(untitled)".into()),
                format!(
                    "state {word}{}{}{}",
                    opt("kind", task.kind.as_deref()),
                    opt("project", task.project.as_deref()),
                    task.priority
                        .map(|p| format!("  priority {p}"))
                        .unwrap_or_default(),
                ),
                format!(
                    "created {}  updated {}",
                    task.created_at.as_str(),
                    task.updated_at.as_str()
                ),
            ])
        }
        BoardTarget::WorkflowRun(id) => {
            let run = state
                .runs
                .iter()
                .find(|run| run.id.as_str() == id.as_str())?;
            Some(vec![
                format!("workflow run {}  version {}", run.id.as_str(), run.version),
                format!(
                    "title {}  state {}",
                    run.title.as_deref().unwrap_or("absent"),
                    run.state,
                ),
                run.step.as_deref().map_or_else(
                    || "step absent -- run has not advanced".to_owned(),
                    |step| format!("step {step}"),
                ),
                run.template_sha256.as_deref().map_or_else(
                    || format!("template {}  sha256 absent", run.template_ref),
                    |digest| format!("template {}  sha256 {digest}", run.template_ref),
                ),
                run.task_id.as_ref().map_or_else(
                    || "task absent".to_owned(),
                    |task_id| format!("task {}", task_id.as_str()),
                ),
                format!(
                    "opened {}  updated {}",
                    run.opened_at.as_str(),
                    run.updated_at.as_str(),
                ),
                run.closed_at.as_ref().map_or_else(
                    || "closed absent -- run is live".to_owned(),
                    |closed| format!("closed {}", closed.as_str()),
                ),
            ])
        }
        BoardTarget::Attempt(id) => {
            let attempt = state
                .attempts
                .iter()
                .find(|a| a.id.as_str() == id.as_str())?;
            let (word, _) = attempt_face(attempt.state);
            let budget = attempt_budget(attempt);
            let mut lines = vec![
                format!(
                    "attempt {}  task {}",
                    attempt.id.as_str(),
                    attempt.task_id.as_str()
                ),
                format!(
                    "engine {}{}{}{}",
                    attempt.engine.as_str(),
                    opt("capability", attempt.capability.as_deref()),
                    opt("role", attempt.role.as_deref()),
                    opt("lane", attempt.model_lane.as_deref()),
                ),
                format!(
                    "state {word}{}{}",
                    attempt
                        .exit_code
                        .map(|c| format!("  exit {c}"))
                        .unwrap_or_default(),
                    attempt
                        .result_valid
                        .map(|v| format!("  result valid {v}"))
                        .unwrap_or_default(),
                ),
            ];
            match budget.budget.as_ref() {
                None => lines.push("budget not recorded -- every axis is uncapped".to_owned()),
                Some(value) => {
                    let mut caps = Vec::new();
                    if let Some(max) = value.max_tokens {
                        caps.push(format!("max tokens {max}"));
                    }
                    if let Some(max) = value.max_tool_calls {
                        caps.push(format!("max tool calls {max}"));
                    }
                    if let Some(max) = value.max_wall_ms {
                        caps.push(format!("max wall {max}ms"));
                    }
                    if let Some(max) = value.max_cost_micros {
                        caps.push(format!("max cost {} micros", max.value()));
                    }
                    lines.push(if caps.is_empty() {
                        "budget recorded -- every axis is uncapped".to_owned()
                    } else {
                        format!("budget  {}", caps.join("  "))
                    });
                    if !budget.uncapped_axes.is_empty() {
                        lines.push(format!("uncapped {}", budget.uncapped_axes.join(", ")));
                    }
                }
            }
            lines.push(format!(
                "created {}  updated {}",
                attempt.created_at.as_str(),
                attempt.updated_at.as_str()
            ));
            Some(lines)
        }
        BoardTarget::Node(id) => {
            let node = state.nodes.iter().find(|n| n.id.as_str() == id.as_str())?;
            let anchor = match (&node.attempt_id, &node.parent_id) {
                (Some(attempt), Some(parent)) => {
                    format!("attempt {}  parent {}", attempt.as_str(), parent.as_str())
                }
                (Some(attempt), None) => format!("attempt {}  parent absent", attempt.as_str()),
                (None, Some(parent)) => format!("attempt absent  parent {}", parent.as_str()),
                (None, None) => "anchor absent -- no attempt or parent".into(),
            };
            Some(vec![
                format!("spawn {}", node.id.as_str()),
                format!(
                    "kind {}  state {}{}",
                    node.kind,
                    node.state,
                    opt("label", node.label.as_deref()),
                ),
                anchor,
                format!(
                    "created {}  updated {}",
                    node.created_at.as_str(),
                    node.updated_at.as_str()
                ),
            ])
        }
        BoardTarget::Message(id) => {
            let message = state
                .messages
                .iter()
                .find(|m| m.id.as_str() == id.as_str())?;
            let (word, _) = message_face(message.state);
            Some(vec![
                format!(
                    "message {}{}{}",
                    message.id.as_str(),
                    opt(
                        "correlation",
                        message.correlation_id.as_ref().map(|c| c.as_str())
                    ),
                    opt("reply-to", message.reply_to.as_ref().map(|r| r.as_str())),
                ),
                format!(
                    "{} -> {}{}{}",
                    message.sender.as_deref().unwrap_or("?"),
                    message.recipient.as_deref().unwrap_or("?"),
                    opt("channel", message.channel.as_deref()),
                    opt("kind", message.kind.as_deref()),
                ),
                format!(
                    "state {word}  tries {}{}",
                    message.delivery_attempts,
                    opt("reason", message.dead_letter_reason.as_deref()),
                ),
                format!(
                    "created {}  updated {}",
                    message.created_at.as_str(),
                    message.updated_at.as_str()
                ),
            ])
        }
        BoardTarget::Event(id) => {
            let event = state
                .events
                .iter()
                .find(|event| event.event_id.as_str() == id.as_str())?;
            Some(vec![
                format!(
                    "event {}  sequence {}  version {}",
                    event.event_id.as_str(),
                    event.global_sequence,
                    event.aggregate_version
                ),
                format!(
                    "{}  {}/{}",
                    event.event_type,
                    event.aggregate_type,
                    event.aggregate_id.as_str()
                ),
                format!(
                    "actor {}{}  origin {}{}",
                    event.actor.kind,
                    opt("id", event.actor.id.as_deref()),
                    event.origin.system,
                    opt("ref", event.origin.r#ref.as_deref()),
                ),
                format!(
                    "occurred {}  appended {}",
                    event.occurred_at.as_str(),
                    event.appended_at.as_str()
                ),
                format!("payload {}", event.payload),
            ])
        }
        BoardTarget::Attention(id) => {
            let item = state
                .attention
                .iter()
                .find(|item| item.id.as_str() == id.as_str())?;
            Some(vec![
                format!("attention {}  {}", item.id.as_str(), item.kind),
                item.summary.clone(),
                format!(
                    "state {}{}{}",
                    if item.resolved_at.is_some() {
                        "resolved"
                    } else {
                        "unresolved"
                    },
                    opt("subject", item.subject_ref.as_deref()),
                    item.priority
                        .map(|priority| format!("  priority {priority}"))
                        .unwrap_or_default(),
                ),
                format!(
                    "raised {}{}{}",
                    item.raised_at.as_str(),
                    opt("acked", item.acked_at.as_ref().map(|at| at.as_str())),
                    opt(
                        "muted-until",
                        item.muted_until.as_ref().map(|at| at.as_str())
                    ),
                ),
            ])
        }
        BoardTarget::Session(id) => {
            let session = state
                .sessions
                .iter()
                .find(|session| session.id.as_str() == id.as_str())?;
            let (word, _) = session_face(session);
            Some(vec![
                format!(
                    "engine session {}  attempt {}",
                    session.id.as_str(),
                    session.attempt_id.as_str()
                ),
                format!(
                    "engine {}{}",
                    session.engine.as_str(),
                    opt("provider ref", session.provider_session_ref.as_deref()),
                ),
                format!("state {word}  started {}", session.started_at.as_str()),
                session.ended_at.as_ref().map_or_else(
                    || "ended -- the log carries no end stamp".to_owned(),
                    |ended| format!("ended {}", ended.as_str()),
                ),
            ])
        }
        BoardTarget::Worktree(id) => {
            let worktree = state
                .worktrees
                .iter()
                .find(|worktree| worktree.id.as_str() == id.as_str())?;
            let (word, _) = worktree_face(worktree);
            Some(vec![
                format!("worktree {}", worktree.id.as_str()),
                format!("{}  {}  {}", worktree.repo, worktree.branch, worktree.path),
                format!(
                    "state {word}  dirty {}  unpushed {}{}{}",
                    worktree.dirty,
                    worktree.unpushed,
                    opt("lease", worktree.lease_id.as_ref().map(|id| id.as_str())),
                    opt("disposition", worktree.disposition.as_deref()),
                ),
                format!(
                    "created {}{}",
                    worktree.created_at.as_str(),
                    worktree
                        .released_at
                        .as_ref()
                        .map(|at| format!("  released {}", at.as_str()))
                        .unwrap_or_default(),
                ),
            ])
        }
        BoardTarget::Lease(id) => {
            let lease = state
                .leases
                .iter()
                .find(|lease| lease.id.as_str() == id.as_str())?;
            let (word, _) = lease_face(lease.state);
            Some(vec![
                format!("lease {}", lease.id.as_str()),
                format!(
                    "state {word}{}{}",
                    opt("holder", lease.holder.as_deref()),
                    opt("scope", lease.scope.as_deref()),
                ),
                format!(
                    "dirty {}  unpushed {}{}{}",
                    lease.dirty,
                    lease.unpushed,
                    opt("fence", lease.fence_token.map(|t| t.to_string()).as_deref()),
                    opt("expires", lease.expires_at.as_ref().map(|at| at.as_str())),
                ),
                format!(
                    "created {}  updated {}",
                    lease.created_at.as_str(),
                    lease.updated_at.as_str()
                ),
            ])
        }
        BoardTarget::Cost(id) => {
            let cost = state
                .costs
                .iter()
                .find(|cost| cost.id.as_str() == id.as_str())?;
            let tokens = |label: &str, value: Option<gwk_domain::ids::TokenCount>| {
                value
                    .map(|count| format!("  {label} {}", count.value()))
                    .unwrap_or_default()
            };
            Some(vec![
                format!("cost entry {}  {}", cost.id.as_str(), cost.engine.as_str()),
                format!(
                    "recorded {}{}",
                    cost.recorded_at.as_str(),
                    opt("model", cost.model.as_deref()),
                ),
                // Absent currency is a sentence, not an empty field: an engine
                // that reports no dollar figure leaves both columns null and
                // the ledger declines to convert for it.
                match (cost.cost_micros, cost.cost_is_estimate) {
                    (Some(value), Some(true)) => {
                        format!(
                            "cost {} (engine-reported estimate)",
                            usd(u128::from(value.value()))
                        )
                    }
                    (Some(value), _) => format!("cost {}", usd(u128::from(value.value()))),
                    (None, _) => "cost not reported -- tokens are the only fact here".to_owned(),
                },
                format!(
                    "tokens{}{}{}{}{}",
                    tokens("input", cost.input_tokens),
                    tokens("cached", cost.cached_input_tokens),
                    tokens("cache-write", cost.cache_write_tokens),
                    tokens("output", cost.output_tokens),
                    tokens("reasoning", cost.reasoning_tokens),
                ),
                format!(
                    "subject{}{}{}",
                    opt("attempt", cost.attempt_id.as_ref().map(|id| id.as_str())),
                    opt(
                        "session",
                        cost.engine_session_id.as_ref().map(|id| id.as_str())
                    ),
                    opt(
                        "spawn",
                        cost.dispatch_node_id.as_ref().map(|id| id.as_str())
                    ),
                ),
            ])
        }
        BoardTarget::Ingested(id) => {
            let record = state
                .ingested
                .iter()
                .find(|record| record.id.as_str() == id.as_str())?;
            Some(vec![
                format!("ingested record {}", record.id.as_str()),
                format!(
                    "kind {}  ingested {}  event seq {}",
                    record.kind,
                    record.ingested_at.as_str(),
                    record.event_seq
                ),
                // The payload has no per-kind schema anywhere in the contract:
                // it is free-form JSON by construction. The pane prints it as
                // the opaque value it is rather than naming fields nothing
                // guarantees.
                format!("payload {}", record.payload),
            ])
        }
        BoardTarget::Receipt(id) => {
            let receipt = state
                .receipts
                .iter()
                .find(|receipt| receipt.id.as_str() == id.as_str())?;
            Some(vec![
                format!("receipt {}  {}", receipt.id.as_str(), receipt.ts.as_str()),
                format!("{} by {}", receipt.action, actor_face(&receipt.actor)),
                format!("subject {} {}", receipt.subject_type, receipt.subject_id),
                match (&receipt.from, &receipt.to) {
                    (Some(from), Some(to)) => format!("edge {from} -> {to}"),
                    (None, Some(to)) => format!("edge -> {to}"),
                    (Some(from), None) => format!("edge {from} ->"),
                    (None, None) => "edge none -- this action moved no state".to_owned(),
                },
                // Absent basis is a sentence: the field is where a flip
                // records what it observed, and a blank line would read as a
                // basis of nothing rather than as a receipt that named none.
                receipt.observed_basis.as_deref().map_or_else(
                    || "observed basis not recorded".to_owned(),
                    |basis| format!("observed basis {basis}"),
                ),
            ])
        }
        BoardTarget::ReplayFrame(seq) => {
            let frame = state
                .replay
                .frames()
                .iter()
                .find(|frame| replay_seq(frame) == *seq)?;
            let elapsed_ms = replay_elapsed_ms(frame);
            Some(match frame {
                ReplayFrame::Output { bytes, .. } => vec![
                    format!("frame {seq}  elapsed {elapsed_ms}ms"),
                    format!("output {} bytes", bytes.len()),
                    format!("preview {}", output_preview(bytes)),
                ],
                ReplayFrame::Resize { cols, rows, .. } => vec![
                    format!("frame {seq}  elapsed {elapsed_ms}ms"),
                    format!("resize {cols}x{rows}"),
                ],
            })
        }
    }
}

/// Paint the Board into `area`, registering every painted actionable row in
/// `hits`.
///
/// The same frame grammar as every lens: column zero is the accent column
/// (the selected row's reverse-video space, tier-independent), the last row
/// is the status bar, a body that overflows names the cut, and cut rows
/// register no hit — neither input path can act on a row the operator
/// cannot see. A selection additionally opens the detail pane above the
/// status bar: full ids and full fields, because the ruled answer to labels
/// that do not fit is a pane, not bigger labels.
pub fn render(
    area: Rect,
    buf: &mut Buffer,
    state: &BoardState,
    selected: Option<&BoardTarget>,
    tier: ColorTier,
    glyphs: GlyphSet,
    hits: &mut HitMap<BoardTarget>,
) {
    render_with_status(area, buf, state, selected, tier, glyphs, hits, None);
}

/// Paint a Board-backed five-lens surface without reviving the retired Board
/// tab strip. The shell owns navigation; this footer keeps only the active
/// surface name, running pulse, and projector watermark.
pub fn render_embedded(
    area: Rect,
    buf: &mut Buffer,
    state: &BoardState,
    selected: Option<&BoardTarget>,
    tier: ColorTier,
    glyphs: GlyphSet,
    hits: &mut HitMap<BoardTarget>,
) {
    render_with_status(
        area,
        buf,
        state,
        selected,
        tier,
        glyphs,
        hits,
        Some(embedded_surface_name(state.view)),
    );
}

fn embedded_surface_name(view: BoardView) -> &'static str {
    match view {
        BoardView::Estate => "hall",
        BoardView::Activity => "queue",
        BoardView::Runs => "runs",
        BoardView::Dag => "tasks",
        BoardView::Flow => "messages",
        BoardView::Events => "events",
        BoardView::Replay => "term",
        BoardView::Fleet => "leases",
        BoardView::CostHealth => "cost",
        BoardView::Audit => "audit",
    }
}

#[allow(clippy::too_many_arguments)]
fn render_with_status(
    area: Rect,
    buf: &mut Buffer,
    state: &BoardState,
    selected: Option<&BoardTarget>,
    tier: ColorTier,
    glyphs: GlyphSet,
    hits: &mut HitMap<BoardTarget>,
    surface_name: Option<&str>,
) {
    hits.clear();
    if area.height == 0 || area.width == 0 {
        return;
    }

    let built = rows(state, tier);
    // Status bar first, then the pane, then the body keeps at least one
    // row: a cramped pane shrinks before the body vanishes, and below two
    // rows — the rule plus one line — the pane is not worth its cells.
    let after_status = area.height - 1;
    let detail = selected
        .and_then(|target| detail_lines(state, target))
        .map(|lines| {
            wrap_detail(
                &lines,
                area.width.saturating_sub(1) as usize,
                usize::from(after_status.min(256)),
            )
        });
    let desired_detail_rows = detail.as_ref().map_or(0, |(lines, source_cut)| {
        let cap = after_status.saturating_sub(1);
        if cap < 2 {
            0
        } else {
            (lines.len() as u16 + 1 + u16::from(*source_cut)).min(cap)
        }
    });
    let selected_row = selected.and_then(|target| {
        built
            .iter()
            .position(|row| row.target.as_ref() == Some(target))
    });
    let diagnostic_rows = built.iter().take_while(|row| row.diagnostic).count();
    let ordinary = &built[diagnostic_rows..];
    let minimum_ordinary_rows = if ordinary.len() > 1 { 2 } else { 1 };
    let minimum_body_rows = u16::try_from(diagnostic_rows)
        .unwrap_or(u16::MAX)
        .saturating_add(minimum_ordinary_rows);
    let max_detail_rows = after_status.saturating_sub(minimum_body_rows);
    let detail_rows = desired_detail_rows.min(max_detail_rows);
    let detail_rows = if detail_rows < 2 { 0 } else { detail_rows };
    let body_rows = after_status - detail_rows;

    // Pinned rows keep first claim on the body, because a corruption finding
    // outranks a row: `board_invalid_page_diagnostics_survive_body_overflow`
    // is the older invariant and it stands. What changed is that neither of
    // the two cuts a short frame forces is silent any more — see `pinned_cut`
    // below and the `ordinary_body_rows` guard on the overflow marker.
    let pinned = diagnostic_rows.min(body_rows as usize);
    let pinned_cut = diagnostic_rows - pinned;
    let ordinary_body_rows = body_rows.saturating_sub(pinned as u16);
    let overflow = ordinary.len().saturating_sub(ordinary_body_rows as usize);
    let visible = if overflow > 0 {
        (ordinary_body_rows as usize).saturating_sub(1)
    } else {
        ordinary.len()
    };

    let selection_fg = match tier {
        ColorTier::Truecolor | ColorTier::Xterm256 => gwk_theme::SIGNAL
            .iter()
            .find(|t| t.name == "selection")
            .map(|t| theme::token_style(t, tier)),
        ColorTier::Ansi16 | ColorTier::Mono => None,
    };

    let start = if visible == 0 {
        0
    } else {
        selected_row
            .map(|index| index.saturating_sub(diagnostic_rows))
            .filter(|index| *index >= visible)
            .map_or(0, |index| index + 1 - visible)
            .min(ordinary.len().saturating_sub(visible))
    };
    let end = (start + visible).min(ordinary.len());

    for (i, row) in built.iter().take(pinned).enumerate() {
        let y = area.y + i as u16;
        // A pinned block too tall for the frame names its own cut in its last
        // row. The block's header states a count, so silently painting fewer
        // rows than it claims is the one failure a pinned block must not have:
        // the operator would read three stated findings and see two.
        let cut = pinned_cut > 0 && i + 1 == pinned;
        let text = if cut {
            std::borrow::Cow::Owned(format!("+{pinned_cut} more pinned -- frame too short"))
        } else {
            theme::safe_text(&row.text, area.width.saturating_sub(1) as usize)
        };
        buf.set_stringn(
            area.x + 1,
            y,
            text.as_ref(),
            area.width.saturating_sub(1) as usize,
            row.style,
        );
    }

    for (i, row) in ordinary[start..end].iter().enumerate() {
        let y = area.y + pinned as u16 + i as u16;
        let is_selected = matches!((selected, &row.target), (Some(sel), Some(t)) if *sel == *t);
        let text_style = match (is_selected, selection_fg) {
            (true, Some(fg)) => row.style.patch(fg),
            _ => row.style,
        };
        if is_selected {
            buf.set_string(
                area.x,
                y,
                ">",
                Style::default().add_modifier(Modifier::BOLD),
            );
        }
        let mut x = area.x + 1 + row.indent.min(INDENT_CAP * INDENT_STEP);
        let depth = row.indent / INDENT_STEP;
        if depth > INDENT_CAP {
            let label = format!("+{depth} ");
            let budget = (area.x + area.width).saturating_sub(x);
            buf.set_stringn(x, y, &label, budget as usize, text_style);
            x = x.saturating_add(u16::try_from(label.len()).unwrap_or(u16::MAX));
        }
        if let Some((mark, mark_style)) = row.mark {
            // Static marks only in this lens; frame 0 is every frame. The
            // mark keeps its own style even on the selected row, and its
            // write is bounded by the area like every other one.
            let glyph = theme::glyph(mark, 0, glyphs);
            let budget = (area.x + area.width).saturating_sub(x);
            buf.set_stringn(x, y, glyph.to_string(), budget as usize, mark_style);
            x = x.saturating_add(2);
        }
        let width = (area.x + area.width).saturating_sub(x);
        let safe_text = theme::safe_text(&row.text, width as usize);
        buf.set_stringn(x, y, safe_text.as_ref(), width as usize, text_style);
        if let Some(right) = &row.right {
            crate::row::paint_tail(
                buf,
                area,
                y,
                x.saturating_add(u16::try_from(safe_text.chars().count()).unwrap_or(u16::MAX)),
                right,
                text_style,
            );
        }
        if let Some(target) = &row.target {
            hits.register(Rect::new(area.x, y, area.width, 1), target.clone());
        }
    }

    // `ordinary_body_rows == 0` means the pinned block took the whole body, so
    // there is no row left to print a marker into. The old code painted one
    // at `area.y + body_rows` anyway — the status-bar row — where the bar
    // overwrote it a moment later. Guarding it changes nothing an operator
    // sees, which is exactly why it is worth saying out loud: the marker was
    // never visible there, so this removes a pointless write rather than
    // fixing a visible bug, and there is no frame assertion that could tell
    // the two versions apart.
    //
    // Recorded ceiling: at a body this short the ordinary rows go unstated —
    // pinned rows keep first claim (the corruption invariant), and there is
    // genuinely no row left for a count. Real frames clear it at one row past
    // the pinned block; `gw board` and `gw event tail` drive live frames well
    // above it.
    if overflow > 0 && ordinary_body_rows > 0 {
        let y = area.y + pinned as u16 + ordinary_body_rows.saturating_sub(1);
        buf.set_stringn(
            area.x + 1,
            y,
            if start == 0 {
                format!("+{} more", ordinary.len() - end)
            } else if end == ordinary.len() {
                format!("+{start} before")
            } else {
                format!("+{start} before  +{} more", ordinary.len() - end)
            },
            area.width.saturating_sub(1) as usize,
            theme::state_style(theme::binding("idle"), tier),
        );
    }

    if let (Some((lines, source_cut)), true) = (&detail, detail_rows > 0) {
        let muted = theme::state_style(theme::binding("idle"), tier);
        let top = area.y + body_rows;
        buf.set_stringn(
            area.x,
            top,
            "-".repeat(area.width as usize),
            area.width as usize,
            muted,
        );
        let capacity = detail_rows.saturating_sub(1) as usize;
        let truncated = *source_cut || lines.len() > capacity;
        let visible_lines = if truncated {
            capacity.saturating_sub(1)
        } else {
            lines.len()
        };
        for (i, line) in lines.iter().take(visible_lines).enumerate() {
            buf.set_stringn(
                area.x + 1,
                top + 1 + i as u16,
                line,
                area.width.saturating_sub(1) as usize,
                Style::default(),
            );
        }
        if truncated && capacity > 0 {
            let omitted = lines.len() - visible_lines;
            let y = top + detail_rows - 1;
            let notice = if *source_cut {
                "+more detail".to_string()
            } else {
                format!("+{omitted} more detail lines")
            };
            let safe_notice = theme::safe_text(&notice, area.width.saturating_sub(1) as usize);
            buf.set_stringn(
                area.x + 1,
                y,
                safe_notice.as_ref(),
                area.width.saturating_sub(1) as usize,
                muted,
            );
        }
    }

    // The status bar: which view this is, the board's pulse, and the page's
    // as-of stamp. Every inactive view's name stays visible — the other
    // panels are one keystroke away, and the bar says so rather than hiding
    // four of five behind a discoverable-by-accident binding.
    let as_of = state
        .watermark
        .as_ref()
        .map_or_else(|| "-".to_string(), |w| w.to_string());
    let status = surface_name.map_or_else(
        || {
            let tabs = BoardView::ALL
                .iter()
                .map(|view| {
                    if *view == state.view {
                        format!("[{}]", view.as_str())
                    } else {
                        view.as_str().to_owned()
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            // `rN@seq` keeps the running-attempt pulse and projector watermark visible
            // after the run-ledger tab joined the 72-cell baseline frame.
            format!("BOARD {tabs}  r{}@{as_of}", running_count(state))
        },
        |name| {
            format!(
                "{}  r{}@{as_of}",
                name.to_ascii_uppercase(),
                running_count(state)
            )
        },
    );
    let safe_status = theme::safe_text(&status, area.width as usize);
    buf.set_stringn(
        area.x,
        area.y + area.height - 1,
        safe_status.as_ref(),
        area.width as usize,
        Style::default(),
    );
}

#[cfg(test)]
mod tests {
    use gwk_domain::entity::{Budget, WorkflowRun};
    use gwk_domain::ids::{
        AggregateId, CorrelationId, CostMicros, EngineId, IdempotencyKey, ProjectId, TokenCount,
        WorkflowRunId,
    };
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    fn ts(s: &str) -> Timestamp {
        Timestamp::new(s)
    }

    fn task(id: &str, title: &str, state: TaskState, updated: &str) -> Task {
        Task {
            id: TaskId::new(id),
            version: 1,
            state,
            kind: None,
            title: Some(title.into()),
            spec_ref: None,
            project: None,
            priority: None,
            tracker_ref: None,
            created_at: ts("2026-08-06T08:00:00Z"),
            updated_at: ts(updated),
        }
    }

    fn attempt(id: &str, task: &str, engine: &str, state: AttemptState, updated: &str) -> Attempt {
        Attempt {
            id: AttemptId::new(id),
            version: 1,
            state,
            task_id: TaskId::new(task),
            engine: EngineId::new(engine),
            capability: None,
            role: None,
            model_lane: None,
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
            created_at: ts("2026-08-06T09:40:00Z"),
            updated_at: ts(updated),
        }
    }

    fn node(
        id: &str,
        attempt: Option<&str>,
        parent: Option<&str>,
        label: &str,
        state: &str,
    ) -> DispatchNode {
        DispatchNode {
            id: DispatchNodeId::new(id),
            version: 1,
            parent_id: parent.map(DispatchNodeId::new),
            attempt_id: attempt.map(AttemptId::new),
            kind: "subagent".into(),
            state: state.into(),
            label: Some(label.into()),
            created_at: ts("2026-08-06T09:45:00Z"),
            updated_at: ts("2026-08-06T09:50:00Z"),
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
            idempotency_key: IdempotencyKey::new(format!("k-{id}")),
            correlation_id: None,
            reply_to: None,
            sender: Some(sender.into()),
            recipient: Some(recipient.into()),
            channel: None,
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

    fn attention_item(
        id: &str,
        summary: &str,
        priority: Option<i32>,
        raised: &str,
        resolved: Option<&str>,
    ) -> AttentionItem {
        AttentionItem {
            id: AttentionItemId::new(id),
            kind: "operator".into(),
            summary: summary.into(),
            subject_ref: Some(format!("attempt:{id}")),
            raised_by: None,
            priority,
            raised_at: ts(raised),
            acked_at: None,
            muted_until: None,
            resolved_at: resolved.map(ts),
            resolution: resolved.map(|_| "handled".into()),
        }
    }

    fn event(id: &str, seq: u64, aggregate: &str, kind: &str, appended: &str) -> EventEnvelope {
        EventEnvelope {
            event_id: EventId::new(id),
            project_id: ProjectId::new("system"),
            aggregate_type: aggregate.into(),
            aggregate_id: AggregateId::new(format!("{aggregate}-{seq}")),
            aggregate_version: 1,
            event_type: kind.into(),
            schema_version: 1,
            global_sequence: Seq::new(seq),
            occurred_at: ts(appended),
            appended_at: ts(appended),
            actor: gwk_domain::envelope::Actor {
                kind: "kernel".into(),
                id: None,
            },
            origin: gwk_domain::envelope::Origin {
                system: "gwk".into(),
                r#ref: None,
            },
            causation_id: None,
            correlation_id: None,
            idempotency_key: None,
            payload: serde_json::json!({"sequence": seq}),
            payload_ref: None,
        }
    }

    fn workflow_run(
        id: &str,
        state: &str,
        step: Option<&str>,
        title: Option<&str>,
        closed_at: Option<&str>,
    ) -> WorkflowRun {
        WorkflowRun {
            id: WorkflowRunId::new(id),
            version: 3,
            state: state.to_owned(),
            step: step.map(str::to_owned),
            template_ref: "delivery@v3".to_owned(),
            template_sha256: None,
            task_id: Some(TaskId::new("task-board")),
            title: title.map(str::to_owned),
            opened_at: ts("2026-08-09T09:00:00Z"),
            updated_at: ts("2026-08-09T09:30:00Z"),
            closed_at: closed_at.map(ts),
        }
    }

    fn empty_state() -> BoardState {
        BoardState {
            view: BoardView::Dag,
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

    fn session(id: &str, attempt: &str, engine: &str, ended: Option<&str>) -> EngineSession {
        EngineSession {
            id: EngineSessionId::new(id),
            attempt_id: AttemptId::new(attempt),
            engine: EngineId::new(engine),
            provider_session_ref: None,
            started_at: ts("2026-08-06T09:40:00Z"),
            ended_at: ended.map(ts),
        }
    }

    fn worktree(id: &str, branch: &str, released: Option<&str>, unpushed: bool) -> Worktree {
        Worktree {
            id: WorktreeId::new(id),
            repo: "gridwork".into(),
            path: "worktrees/lens".into(),
            branch: branch.into(),
            base_sha: None,
            lease_id: Some(LeaseId::new("ls-01")),
            dirty: false,
            unpushed,
            released_at: released.map(ts),
            disposition: None,
            created_at: ts("2026-08-06T09:00:00Z"),
        }
    }

    fn lease(id: &str, state: LeaseState, holder: &str) -> Lease {
        Lease {
            id: LeaseId::new(id),
            version: 1,
            state,
            mode: gwk_domain::fsm::LeaseMode::Exclusive,
            holder: Some(holder.into()),
            scope: Some("worktree:lens".into()),
            repo: None,
            path: None,
            branch: None,
            base_sha: None,
            fence_token: None,
            heartbeat_at: None,
            expires_at: None,
            dirty: false,
            unpushed: false,
            disposition: None,
            created_at: ts("2026-08-06T09:00:00Z"),
            updated_at: ts("2026-08-06T09:30:00Z"),
        }
    }

    fn cost(
        id: &str,
        engine: &str,
        model: Option<&str>,
        micros: Option<u64>,
        estimate: Option<bool>,
        recorded: &str,
    ) -> CostEntry {
        CostEntry {
            id: CostEntryId::new(id),
            attempt_id: Some(AttemptId::new("at-01")),
            engine_session_id: None,
            dispatch_node_id: None,
            engine: EngineId::new(engine),
            model: model.map(Into::into),
            input_tokens: Some(TokenCount::new(900)),
            cached_input_tokens: None,
            cache_write_tokens: None,
            output_tokens: Some(TokenCount::new(120)),
            reasoning_tokens: None,
            cost_micros: micros.map(CostMicros::new),
            cost_is_estimate: estimate,
            recorded_at: ts(recorded),
        }
    }

    fn ingested(id: &str, kind: IngestionKind, at: &str, seq: u64) -> IngestedRecord {
        IngestedRecord {
            id: IngestedRecordId::new(id),
            kind,
            payload: serde_json::json!({"session": "es-01"}),
            payload_ref: None,
            ingested_by: None,
            event_seq: Seq::new(seq),
            ingested_at: ts(at),
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
            actor: gwk_domain::envelope::Actor {
                kind: "orchestrator".into(),
                id: Some("gw".into()),
            },
            action: action.into(),
            subject_type: subject.0.into(),
            subject_id: subject.1.into(),
            from: edge.0.map(Into::into),
            to: edge.1.map(Into::into),
            observed_basis: basis.map(Into::into),
            ts: ts(at),
        }
    }

    fn audit_state() -> BoardState {
        let mut state = empty_state();
        state.view = BoardView::Audit;
        state.watermark = Some(Seq::new(407));
        state.receipts = vec![
            receipt(
                "rc-01",
                "state_flip",
                ("attempt", "at-01"),
                (Some("queued"), Some("running")),
                Some("engine reported a pid"),
                "2026-08-06T09:41:00Z",
            ),
            receipt(
                "rc-02",
                "auto_answer",
                ("gate", "g-1"),
                (None, None),
                None,
                "2026-08-06T09:52:00Z",
            ),
        ];
        state
    }

    fn fleet_state() -> BoardState {
        let mut state = workday_state();
        state.view = BoardView::Fleet;
        state.sessions = vec![
            session("es-01", "at-01", "codex", None),
            session("es-02", "at-03", "codex", Some("2026-08-06T08:40:00Z")),
        ];
        state.worktrees = vec![
            worktree("wt-01", "feat/auth", None, true),
            worktree("wt-02", "fix/docs", Some("2026-08-06T09:10:00Z"), false),
        ];
        state.leases = vec![
            lease("ls-01", LeaseState::Held, "gw-implementer"),
            lease("ls-02", LeaseState::Expired, "gw-reviewer"),
        ];
        state
    }

    fn cost_state() -> BoardState {
        let mut state = empty_state();
        state.view = BoardView::CostHealth;
        state.watermark = Some(Seq::new(407));
        state.costs = vec![
            cost(
                "ce-01",
                "claude",
                Some("sonnet"),
                Some(1_240_000),
                Some(true),
                "2026-08-06T09:41:00Z",
            ),
            cost("ce-02", "codex", None, None, None, "2026-08-06T09:52:00Z"),
        ];
        state.ingested = vec![ingested(
            "ingest:system:session-1",
            IngestionKind::Session,
            "2026-08-06T09:51:00Z",
            88,
        )];
        state
    }

    fn summary_state() -> BoardState {
        let mut state = workday_state();
        state.view = BoardView::Estate;
        state.tasks[1].state = TaskState::InputRequired;
        state.attempts[1].state = AttemptState::Blocked;
        state.attention = vec![
            attention_item(
                "ai-low",
                "review the late result",
                Some(3),
                "2026-08-06T10:20:00Z",
                None,
            ),
            attention_item(
                "ai-p0",
                "operator decision required",
                Some(0),
                "2026-08-06T10:19:00Z",
                None,
            ),
        ];
        state.worktrees = vec![worktree("wt-01", "feat/auth", None, true)];
        state.leases = vec![lease("ls-01", LeaseState::Held, "gw-implementer")];
        state.costs = vec![
            cost(
                "ce-01",
                "claude",
                Some("sonnet"),
                Some(125_000),
                Some(true),
                "2026-08-06T10:16:00Z",
            ),
            cost("ce-02", "codex", None, None, None, "2026-08-06T10:17:00Z"),
        ];
        state
    }

    fn event_state() -> BoardState {
        let mut state = empty_state();
        state.view = BoardView::Events;
        state.watermark = Some(Seq::new(43));
        state.event_tail = EventTail {
            cursor: Some(Seq::new(40)),
            aggregate_type: Some("attempt".into()),
            event_type: None,
            live: true,
            dropped: 2,
        };
        state.events = vec![
            event(
                "ev-41",
                41,
                "attempt",
                "attempt_started",
                "2026-08-06T10:21:00Z",
            ),
            event(
                "ev-42",
                42,
                "task",
                "task_completed",
                "2026-08-06T10:22:00Z",
            ),
            event(
                "ev-43",
                43,
                "attempt",
                "attempt_succeeded",
                "2026-08-06T10:23:00Z",
            ),
        ];
        state
    }

    fn workday_state() -> BoardState {
        let mut lead = attempt(
            "at-01",
            "t-auth",
            "codex",
            AttemptState::Running,
            "2026-08-06T10:12:00Z",
        );
        lead.role = Some("implementer".into());
        let mut second = attempt(
            "at-02",
            "t-auth",
            "claude",
            AttemptState::Failed,
            "2026-08-06T09:48:00Z",
        );
        second.exit_code = Some(1);
        let shipped = attempt(
            "at-03",
            "t-ship",
            "codex",
            AttemptState::Succeeded,
            "2026-08-06T08:39:00Z",
        );
        // The seeded orphan: its task is beyond this page.
        let orphan = attempt(
            "at-99",
            "t-gone",
            "codex",
            AttemptState::Running,
            "2026-08-06T10:00:00Z",
        );

        let mut brief = message(
            "m-1",
            "orchestrator",
            "researcher",
            "brief",
            MessageState::Delivered,
            "2026-08-06T09:30:00Z",
            "2026-08-06T09:31:00Z",
        );
        brief.correlation_id = Some(CorrelationId::new("c-42"));
        let mut findings = message(
            "m-2",
            "researcher",
            "orchestrator",
            "findings",
            MessageState::Applied,
            "2026-08-06T09:55:00Z",
            "2026-08-06T09:58:00Z",
        );
        findings.correlation_id = Some(CorrelationId::new("c-42"));
        findings.reply_to = Some(MessageId::new("m-1"));
        let mut dead = message(
            "m-3",
            "watchdog",
            "orchestrator",
            "alert",
            MessageState::DeadLetter,
            "2026-08-06T10:02:00Z",
            "2026-08-06T10:05:00Z",
        );
        dead.dead_letter_reason = Some("nobody listening".into());
        dead.delivery_attempts = 3;
        let pending = message(
            "m-4",
            "orchestrator",
            "reviewer",
            "review-request",
            MessageState::Accepted,
            "2026-08-06T10:07:00Z",
            "2026-08-06T10:07:00Z",
        );

        BoardState {
            view: BoardView::Dag,
            tasks: vec![
                task(
                    "t-auth",
                    "harden the auth path",
                    TaskState::Working,
                    "2026-08-06T10:12:00Z",
                ),
                task(
                    "t-docs",
                    "write the release notes",
                    TaskState::Submitted,
                    "2026-08-06T09:05:00Z",
                ),
                task(
                    "t-ship",
                    "ship 0.0.2",
                    TaskState::Completed,
                    "2026-08-06T08:40:00Z",
                ),
            ],
            runs: Vec::new(),
            attempts: vec![lead, second, shipped, orphan],
            nodes: vec![
                node("d-1", Some("at-01"), None, "recon", "completed"),
                node("d-2", Some("at-01"), Some("d-1"), "lint", "registered"),
            ],
            messages: vec![brief, findings, dead, pending],
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
            watermark: Some(Seq::new(407)),
        }
    }

    fn dump_frame(
        w: u16,
        h: u16,
        state: &BoardState,
        selected: Option<&BoardTarget>,
    ) -> (String, HitMap<BoardTarget>, Buffer) {
        dump_frame_tier(w, h, state, selected, ColorTier::Mono)
    }

    fn dump_frame_tier(
        w: u16,
        h: u16,
        state: &BoardState,
        selected: Option<&BoardTarget>,
        tier: ColorTier,
    ) -> (String, HitMap<BoardTarget>, Buffer) {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).expect("terminal");
        let mut hits = HitMap::new();
        terminal
            .draw(|frame| {
                let area = frame.area();
                render(
                    area,
                    frame.buffer_mut(),
                    state,
                    selected,
                    tier,
                    GlyphSet::Unicode,
                    &mut hits,
                );
            })
            .expect("draw");
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            let mut line = String::new();
            for x in 0..buf.area.width {
                line.push_str(buf[(x, y)].symbol());
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        (out, hits, buf)
    }

    fn assert_matches_golden(name: &str, rendered: &str) {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("goldens")
            .join(format!("{name}.txt"));
        let bless = std::env::var_os("BLESS").is_some();
        if bless {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).expect("create goldens dir");
            }
            std::fs::write(&path, rendered).expect("write golden");
        } else {
            let committed = std::fs::read_to_string(&path)
                .unwrap_or_else(|why| panic!("{}: {why} (BLESS=1 to create)", path.display()));
            if committed != rendered {
                let at = committed
                    .lines()
                    .zip(rendered.lines())
                    .position(|(a, b)| a != b);
                panic!(
                    "{} drifted at line {:?}.\n  golden:   {:?}\n  rendered: {:?}\nIf the \
                     frame is right, re-run with BLESS=1.",
                    path.display(),
                    at.map(|l| l + 1),
                    at.and_then(|l| committed.lines().nth(l)),
                    at.and_then(|l| rendered.lines().nth(l)),
                );
            }
        }
        assert!(
            !bless,
            "BLESS=1 rewrites the goldens; it is not a passing run"
        );
    }

    #[test]
    fn board_the_all_none_frame_renders_words_not_blank() {
        let (dump, _, _) = dump_frame(72, 6, &empty_state(), None);
        assert!(
            dump.contains("nothing on the board -- no work dispatched"),
            "absence in words:\n{dump}"
        );
        assert!(
            dump.contains("@-"),
            "an empty log's watermark is said, not skipped:\n{dump}"
        );
        assert_matches_golden("board-empty", &dump);

        let mut flow = empty_state();
        flow.view = BoardView::Flow;
        let (dump, _, _) = dump_frame(72, 6, &flow, None);
        assert!(
            dump.contains("no message flows -- nothing sent"),
            "the flow view's absence has its own words:\n{dump}"
        );
    }

    #[test]
    fn board_a_live_workflow_run_renders_its_open_step_verbatim() {
        let mut state = empty_state();
        state.view = BoardView::Runs;
        state.runs.push(workflow_run(
            "wfr-live",
            "running",
            Some("peer-signoff"),
            Some("ship the Board"),
            None,
        ));
        let target = BoardTarget::WorkflowRun(WorkflowRunId::new("wfr-live"));

        let (dump, hits, _) = dump_frame(88, 16, &state, Some(&target));
        assert!(
            dump.contains("ship the Board  running  step peer-signoff"),
            "the open step must pass through as template data:\n{dump}"
        );
        assert!(dump.contains("closed absent -- run is live"), "{dump}");
        assert!(
            hits.targets().any(|candidate| *candidate == target),
            "the live run must open its detail pane:\n{dump}"
        );

        state.runs[0].step = None;
        let (dump, _, _) = dump_frame(88, 10, &state, None);
        assert!(
            dump.contains("step absent"),
            "a run that has not advanced must say the step is absent:\n{dump}"
        );
    }

    #[test]
    fn board_a_closed_workflow_run_renders_its_outcome_and_close_stamp() {
        let mut state = empty_state();
        state.view = BoardView::Runs;
        state.runs.push(workflow_run(
            "wfr-closed",
            "failed",
            Some("unconventional-finalizer"),
            None,
            Some("2026-08-09T09:45:00Z"),
        ));
        let target = BoardTarget::WorkflowRun(WorkflowRunId::new("wfr-closed"));

        let (dump, _, _) = dump_frame(88, 14, &state, Some(&target));
        assert!(dump.contains("wfr-closed  outcome failed"), "{dump}");
        assert!(dump.contains("closed 2026-08-09T09:45:00Z"), "{dump}");
        assert!(
            dump.contains("step unconventional-finalizer"),
            "a closed row must preserve the step the ledger holds:\n{dump}"
        );
    }

    #[test]
    fn board_an_empty_workflow_run_projection_says_so_in_words() {
        let mut state = empty_state();
        state.view = BoardView::Runs;

        let (dump, hits, _) = dump_frame(88, 8, &state, None);
        assert!(
            dump.contains("no workflow runs -- no run history on this page"),
            "absence must be explicit:\n{dump}"
        );
        assert_eq!(hits.targets().count(), 0, "an empty ledger has no targets");
    }

    #[test]
    fn estate_overview_ranks_attention_without_relabeling_recency() {
        let summary = estate_overview(&summary_state());
        assert_eq!(summary.kind, "estate_overview");
        assert_eq!(summary.watermark, Some(Seq::new(407)));
        assert_eq!(summary.counts.tasks, 3);
        assert_eq!(summary.counts.active_tasks, 2);
        assert_eq!(summary.counts.attempts, 4);
        assert_eq!(summary.counts.running_attempts, 2);
        assert_eq!(summary.counts.unresolved_attention, 2);
        assert_eq!(summary.counts.held_worktrees, 1);
        assert_eq!(summary.counts.held_leases, 1);
        assert_eq!(
            summary.attention_head.expect("attention head").id,
            AttentionItemId::new("ai-p0"),
            "the head is priority-ranked"
        );
        assert_eq!(
            summary.recent_activity[0].id, "ai-low",
            "recent activity remains time-ordered"
        );
        assert!(summary.findings.is_empty());
        assert!(summary.unknowns.is_empty());
    }

    #[test]
    fn activity_brief_names_owed_facts_and_cost_coverage() {
        let summary = activity_brief(&summary_state());
        assert_eq!(summary.kind, "activity_brief");
        assert_eq!(summary.owed_total, 5);
        assert_eq!(summary.owed.len(), 5);
        assert!(
            summary
                .owed
                .iter()
                .any(|fact| fact.kind == "task" && fact.reason == "input required")
        );
        assert!(
            summary
                .owed
                .iter()
                .any(|fact| fact.kind == "attempt" && fact.reason == "blocked")
        );
        assert_eq!(summary.cost.entries, 2);
        assert_eq!(summary.cost.priced_entries, 1);
        assert_eq!(summary.cost.unpriced_entries, 1);
        assert_eq!(summary.cost.estimated_entries, 1);
        assert_eq!(summary.cost.cost_micros.as_deref(), Some("125000"));
    }

    #[test]
    fn cost_rollup_preserves_coverage_and_says_when_the_ledger_is_empty() {
        let summary = cost_rollup(&cost_state());
        assert_eq!(summary.kind, "cost_rollup");
        assert_eq!(summary.watermark, Some(Seq::new(407)));
        assert_eq!(summary.headline.entries, 2);
        assert_eq!(summary.headline.priced_entries, 1);
        assert_eq!(summary.headline.unpriced_entries, 1);
        assert_eq!(summary.headline.estimated_entries, 1);
        assert_eq!(summary.headline.cost_micros.as_deref(), Some("1240000"));
        assert_eq!(summary.by_engine.len(), 2);
        assert_eq!(summary.tokens.input.total.as_deref(), Some("1800"));
        assert_eq!(summary.tokens.input.reported_entries, 2);
        assert_eq!(summary.tokens.reasoning.total, None);
        assert_eq!(summary.entries[0].id, CostEntryId::new("ce-02"));
        assert!(
            summary
                .unknowns
                .iter()
                .any(|note| note.subject == "currency")
        );

        let empty = cost_rollup(&empty_state());
        assert_eq!(empty.headline.entries, 0);
        assert_eq!(empty.headline.cost_micros, None);
        assert!(
            empty
                .unknowns
                .iter()
                .any(|note| note.why.contains("no entries -- no spend recorded")),
            "an empty projection must say what is absent: {empty:?}"
        );
    }

    #[test]
    fn board_cost_with_only_unpriced_entries_never_invents_zero_currency() {
        let mut state = cost_state();
        state.costs.retain(|cost| cost.cost_micros.is_none());
        let (dump, _, _) = dump_frame(72, 18, &state, None);
        assert!(dump.contains("no cost reported"), "{dump}");
        assert!(!dump.contains("0.000000 USD"), "{dump}");
    }

    #[test]
    fn attempt_budget_view_preserves_absence_and_names_each_uncapped_axis() {
        let absent = attempt_budget(&attempt(
            "at-none",
            "t-auth",
            "codex",
            AttemptState::Running,
            "2026-08-06T10:12:00Z",
        ));
        assert_eq!(absent.kind, "attempt_budget");
        assert_eq!(absent.attempt_id, AttemptId::new("at-none"));
        assert_eq!(absent.budget, None);
        assert_eq!(absent.uncapped_axes.len(), 4);

        let mut capped = attempt(
            "at-capped",
            "t-auth",
            "codex",
            AttemptState::Running,
            "2026-08-06T10:12:00Z",
        );
        capped.version = 7;
        capped.budget = Some(Budget {
            max_tokens: Some(1_000),
            max_tool_calls: None,
            max_wall_ms: Some(60_000),
            max_cost_micros: Some(CostMicros::new(250_000)),
        });
        let summary = attempt_budget(&capped);
        assert_eq!(summary.version, 7);
        assert_eq!(summary.uncapped_axes, vec!["max_tool_calls"]);
        assert_eq!(summary.budget, capped.budget);
    }

    #[test]
    fn board_attempt_detail_shows_recorded_caps_and_uncapped_axes() {
        let mut state = workday_state();
        state.attempts[0].budget = Some(Budget {
            max_tokens: Some(1_000),
            max_tool_calls: None,
            max_wall_ms: Some(60_000),
            max_cost_micros: Some(CostMicros::new(250_000)),
        });
        let target = BoardTarget::Attempt(AttemptId::new("at-01"));
        let (dump, _, _) = dump_frame(96, 18, &state, Some(&target));
        assert!(dump.contains("max tokens 1000"), "{dump}");
        assert!(dump.contains("max wall 60000ms"), "{dump}");
        assert!(dump.contains("max cost 250000 micros"), "{dump}");
        assert!(dump.contains("uncapped max_tool_calls"), "{dump}");
        assert_matches_golden("board-attempt-budget", &dump);
    }

    #[test]
    fn board_attempt_verbs_build_only_the_contracts_the_selected_row_supports() {
        let state = workday_state();
        let attempt = BoardTarget::Attempt(AttemptId::new("at-01"));
        assert_eq!(
            stop_attempt(&attempt),
            Some(gwk_domain::command::KernelCommand::IssueCommand {
                command_id: gwk_domain::ids::CommandId::new("stop-attempt:at-01"),
                kind: "stop_attempt".to_owned(),
                targets: vec!["at-01".to_owned()],
                actor: None,
            })
        );
        assert_eq!(
            stop_attempt(&BoardTarget::Task(TaskId::new("t-auth"))),
            None,
            "a task row is not an attempt-targeted stop"
        );

        let budget = Budget {
            max_tokens: Some(1_000),
            max_tool_calls: Some(20),
            max_wall_ms: None,
            max_cost_micros: None,
        };
        assert_eq!(
            update_attempt_budget(&state, &attempt, budget.clone()),
            Some(gwk_domain::command::KernelCommand::UpdateBudget {
                attempt_id: AttemptId::new("at-01"),
                expected_version: 1,
                budget,
            })
        );
        assert_eq!(
            update_attempt_budget(
                &state,
                &BoardTarget::Attempt(AttemptId::new("at-gone")),
                Budget {
                    max_tokens: None,
                    max_tool_calls: None,
                    max_wall_ms: None,
                    max_cost_micros: None,
                },
            ),
            None,
            "a stale row cannot invent the expected version"
        );
    }

    #[test]
    fn summaries_say_when_projection_pages_are_incomplete() {
        let mut state = summary_state();
        state.complete = false;
        let estate = estate_overview(&state);
        let activity = activity_brief(&state);
        assert!(!estate.complete && !activity.complete);
        assert!(estate.unknowns[0].contains("counts are floors"));
        assert!(activity.unknowns[0].contains("page-scoped"));
    }

    #[test]
    fn board_summary_views_render_the_shared_values_and_targets() {
        let state = summary_state();
        let (estate, hits, _) = dump_frame(96, 22, &state, None);
        assert!(estate.contains("operator decision required"), "{estate}");
        assert!(estate.contains("recent activity"), "{estate}");
        assert!(
            hits.targets()
                .any(|target| *target == BoardTarget::Attention(AttentionItemId::new("ai-p0"))),
            "the attention head opens its detail pane:\n{estate}"
        );

        let mut state = state;
        state.view = BoardView::Activity;
        let (activity, _, _) = dump_frame(96, 22, &state, None);
        assert!(activity.contains("what happened"), "{activity}");
        assert!(activity.contains("what is owed  5 facts"), "{activity}");
        assert!(
            activity.contains("2 entries  1 priced  1 unpriced  125000 micros"),
            "{activity}"
        );
    }

    #[test]
    fn board_event_tail_applies_cursor_and_exact_filters() {
        let state = event_state();
        let (dump, hits, _) = dump_frame(96, 16, &state, None);
        assert!(
            dump.contains("2 events  live after 40  filter aggregate=attempt event=*"),
            "{dump}"
        );
        assert!(dump.contains("#43  10:23  attempt_succeeded"), "{dump}");
        assert!(dump.contains("#41  10:21  attempt_started"), "{dump}");
        assert!(!dump.contains("task_completed"), "{dump}");
        assert!(
            dump.contains("2 older events removed from memory"),
            "{dump}"
        );
        assert!(
            hits.targets()
                .any(|target| *target == BoardTarget::Event(EventId::new("ev-43"))),
            "an event row opens its detail pane:\n{dump}"
        );
    }

    #[test]
    fn board_event_detail_keeps_envelope_provenance() {
        let target = BoardTarget::Event(EventId::new("ev-43"));
        let (dump, _, _) = dump_frame(96, 16, &event_state(), Some(&target));
        for expected in [
            "event ev-43  sequence 43  version 1",
            "attempt_succeeded  attempt/attempt-43",
            "actor kernel  origin gwk",
            "payload {\"sequence\":43}",
        ] {
            assert!(dump.contains(expected), "missing {expected:?}:\n{dump}");
        }
    }

    #[test]
    fn board_fleet_says_unended_rather_than_alive() {
        let (dump, hits, _) = dump_frame(72, 24, &fleet_state(), None);
        assert!(
            dump.contains("no end recorded"),
            "a session with no end stamp is unended, never asserted alive:\n{dump}"
        );
        assert!(
            dump.contains("liveness: no end stamp on 1 of 2 sessions -- unended is not alive"),
            "the pinned block names the unknown and counts it:\n{dump}"
        );
        // The row for the unended session may say what the log holds and
        // nothing more. Every word below would be a claim about a process
        // this client has never observed.
        let unended_row = dump
            .lines()
            .find(|line| line.contains("es-01"))
            .unwrap_or_else(|| panic!("the unended session has no row:\n{dump}"));
        for claim in ["alive", "live", "healthy", "up", "running"] {
            assert!(
                !unended_row.contains(claim),
                "the unended session's row claims {claim:?}:\n{unended_row}"
            );
        }
        assert!(
            hits.targets()
                .any(|t| *t == BoardTarget::Session(EngineSessionId::new("es-01"))),
            "a session row is clickable into the detail pane:\n{dump}"
        );
    }

    #[test]
    fn agent_fleet_is_the_typed_value_behind_the_board_panel() {
        let summary = agent_fleet(&fleet_state());
        assert_eq!(summary.kind, "agent_fleet");
        assert_eq!(summary.watermark, Some(Seq::new(407)));
        assert_eq!(summary.counts.sessions, 2);
        assert_eq!(summary.counts.unended_sessions, 1);
        assert_eq!(summary.counts.running_attempts, 2);
        assert_eq!(summary.counts.attempts_without_session, 1);
        assert_eq!(summary.counts.spawns, 2);
        assert_eq!(summary.counts.held_worktrees, 1);
        assert_eq!(summary.counts.held_leases, 1);
        assert_eq!(summary.sessions.len(), 2);
        assert_eq!(summary.dispatch_nodes.len(), 2);
        assert!(
            summary
                .unknowns
                .iter()
                .any(|unknown| unknown.subject == "liveness")
        );
    }

    #[test]
    fn board_fleet_pins_its_unknowns_above_the_rows_it_can_lose() {
        // The unknown block is worthless if a long fleet scrolls it away, so
        // it is pinned like an invalid-page finding. Three body rows is well
        // under what the fixture needs, which is the point of the check.
        let (dump, _, _) = dump_frame(72, 6, &fleet_state(), None);
        assert!(
            dump.contains("unknown -- 2 facts not in the log"),
            "the unknown block survives a frame too small for the rows:\n{dump}"
        );
    }

    #[test]
    fn board_an_empty_fleet_says_so_in_words() {
        let mut state = empty_state();
        state.view = BoardView::Fleet;
        let (dump, _, _) = dump_frame(72, 8, &state, None);
        assert!(
            dump.contains("no fleet -- no sessions, worktrees, or leases on this page"),
            "absence in words:\n{dump}"
        );
        // Zero sessions raise no liveness question, so the panel invents no
        // unknown to look thorough.
        assert!(
            !dump.contains("not in the log"),
            "an empty page has nothing unknown about it:\n{dump}"
        );
    }

    #[test]
    fn board_cost_reports_tokens_when_the_engine_reported_no_currency() {
        let (dump, _, _) = dump_frame(72, 24, &cost_state(), None);
        assert!(
            dump.contains("no cost reported"),
            "an unpriced entry says so rather than printing 0.000000:\n{dump}"
        );
        assert!(
            dump.contains("currency: tokens only on 1 of 2 entries -- the ledger never converts"),
            "the pinned block counts what carries no currency:\n{dump}"
        );
        assert!(
            dump.contains("reasoning not reported"),
            "a token column no row reported is not a zero:\n{dump}"
        );
        assert!(
            dump.contains("input 1800 total over 2 of 2 entries"),
            "a reported column carries its sum and its coverage:\n{dump}"
        );
    }

    #[test]
    fn board_cost_over_a_partial_read_is_a_floor_and_says_which() {
        let complete = cost_state();
        let (whole, _, _) = dump_frame(72, 24, &complete, None);
        assert!(
            whole.contains("1.240000 USD total over 1 of 2 priced"),
            "a read to exhaustion is a total:\n{whole}"
        );
        assert!(
            !whole.contains("figures are floors"),
            "a complete read raises no floor caveat:\n{whole}"
        );

        // The mutation: the same rows, read short. Nothing about the ledger
        // changed — only the caller's claim about how much of it it saw.
        let mut partial = complete;
        partial.complete = false;
        let (cut, _, _) = dump_frame(72, 24, &partial, None);
        assert!(
            cut.contains("1.240000 USD at least over 1 of 2 priced"),
            "a partial read is a floor, not a total:\n{cut}"
        );
        assert!(
            cut.contains("the ledger: read short of the last page -- figures are floors"),
            "and the pinned block says why:\n{cut}"
        );
    }

    #[test]
    fn board_health_with_no_records_is_unknown_and_never_healthy() {
        let (dump, _, _) = dump_frame(72, 24, &cost_state(), None);
        assert!(
            dump.contains("health 0 records"),
            "the count is stated:\n{dump}"
        );
        assert!(
            dump.contains("health: no records -- ingestion is operator-driven, no producer"),
            "and the absence is explained rather than left to read as healthy:\n{dump}"
        );
        assert!(
            dump.contains("session 1 record  newest 2026-08-06T09:51:00Z"),
            "the kind that does have rows carries its newest stamp:\n{dump}"
        );
    }

    #[test]
    fn board_every_new_target_opens_a_detail_pane() {
        for (state, target, expected) in [
            (
                fleet_state(),
                BoardTarget::Session(EngineSessionId::new("es-01")),
                "ended -- the log carries no end stamp",
            ),
            (
                fleet_state(),
                BoardTarget::Worktree(WorktreeId::new("wt-01")),
                "state held  dirty false  unpushed true",
            ),
            (
                fleet_state(),
                BoardTarget::Lease(LeaseId::new("ls-02")),
                "state expired",
            ),
            (
                cost_state(),
                BoardTarget::Cost(CostEntryId::new("ce-02")),
                "cost not reported -- tokens are the only fact here",
            ),
            (
                cost_state(),
                BoardTarget::Ingested(IngestedRecordId::new("ingest:system:session-1")),
                "kind session",
            ),
        ] {
            let (dump, _, _) = dump_frame(78, 24, &state, Some(&target));
            assert!(
                dump.contains(expected),
                "{target:?} detail pane missing {expected:?}:\n{dump}"
            );
        }
    }

    #[test]
    fn board_audit_prints_the_edge_and_names_a_missing_basis() {
        let (dump, hits, _) = dump_frame(78, 22, &audit_state(), None);
        assert!(
            dump.contains("2 receipts  total across 2 actions"),
            "the summary folds the ledger:\n{dump}"
        );
        assert!(
            dump.contains("09:41  state_flip  attempt at-01  queued -> running"),
            "a state flip prints its edge, which is its whole content:\n{dump}"
        );
        assert!(
            dump.contains("basis: no observed basis on 1 of 2 receipts"),
            "a missing basis is counted in the pinned block:\n{dump}"
        );
        assert!(
            hits.targets()
                .any(|t| *t == BoardTarget::Receipt(ReceiptId::new("rc-01"))),
            "a receipt row opens the detail pane:\n{dump}"
        );

        let selected = BoardTarget::Receipt(ReceiptId::new("rc-02"));
        let (dump, _, _) = dump_frame(78, 22, &audit_state(), Some(&selected));
        for expected in [
            "auto_answer by orchestrator:gw",
            "edge none -- this action moved no state",
            "observed basis not recorded",
        ] {
            assert!(dump.contains(expected), "missing {expected:?}:\n{dump}");
        }
    }

    #[test]
    fn board_an_empty_audit_ledger_says_so_in_words() {
        let mut state = empty_state();
        state.view = BoardView::Audit;
        let (dump, _, _) = dump_frame(72, 8, &state, None);
        assert!(
            dump.contains("no receipts -- nothing attested on this page"),
            "absence in words:\n{dump}"
        );
    }

    #[test]
    fn board_a_short_frame_never_scribbles_on_the_status_bar_or_cuts_in_silence() {
        // Two regressions, both from a pinned block taller than the body. The
        // fleet and cost panels emit two to four pinned rows on an ORDINARY
        // frame, so this stopped being the rare invalid-page geometry it was.
        let mut state = fleet_state();
        state.complete = false;
        // One header plus three notes; no selection, so no detail pane and
        // the body is `height - 1`. The block therefore fits from height 5.
        const PINNED: u16 = 4;

        for height in 2..=20u16 {
            let (dump, _, _) = dump_frame(72, height, &state, None);
            let lines: Vec<&str> = dump.lines().collect();

            // The status bar survives every geometry. Deliberately NOT the
            // regression test for the stray marker write: the bar always
            // painted last and overwrote it, so no frame assertion can tell
            // the guarded and unguarded versions apart — verified by mutation.
            assert!(
                lines
                    .get(usize::from(height - 1))
                    .is_some_and(|bar| bar.starts_with("BOARD")),
                "height {height} lost the status bar:\n{dump}"
            );

            // The finding this DOES pin: a pinned block that does not fit says
            // how much it lost. Its
            //    header states a count, so showing fewer rows than it claims
            //    is the one failure a pinned block must not have.
            let cut = dump.contains("more pinned -- frame too short");
            assert_eq!(
                cut,
                height - 1 < PINNED,
                "height {height}: pinned-cut notice {} when the body holds {} of {PINNED}:\n{dump}",
                if cut { "shown" } else { "absent" },
                height - 1,
            );
        }

        // 3. Where a body row exists for it, the ordinary block's own marker
        //    is present rather than lost off the bottom.
        let (dump, _, _) = dump_frame(72, 7, &state, None);
        assert!(
            dump.contains("more"),
            "a frame with a body row must name the rows it cut:\n{dump}"
        );
        let (dump, _, _) = dump_frame(72, 20, &state, None);
        assert!(
            dump.contains("engine sessions") && dump.contains("leases"),
            "a tall frame paints its rows and raises no cut:\n{dump}"
        );
    }

    #[test]
    fn board_an_audit_frame_matches_its_golden() {
        let (dump, _, _) = dump_frame(78, 16, &audit_state(), None);
        assert_matches_golden("board-audit", &dump);
    }

    #[test]
    fn board_a_fleet_frame_matches_its_golden() {
        let (dump, _, _) = dump_frame(72, 22, &fleet_state(), None);
        assert_matches_golden("board-fleet", &dump);
    }

    #[test]
    fn board_a_cost_frame_matches_its_golden() {
        let (dump, _, _) = dump_frame(72, 24, &cost_state(), None);
        assert_matches_golden("board-cost", &dump);
    }

    #[test]
    fn board_an_estate_frame_matches_its_golden() {
        let (dump, _, _) = dump_frame(96, 18, &summary_state(), None);
        assert_matches_golden("board-estate", &dump);
    }

    #[test]
    fn board_an_activity_frame_matches_its_golden() {
        let mut state = summary_state();
        state.view = BoardView::Activity;
        let (dump, _, _) = dump_frame(96, 18, &state, None);
        assert_matches_golden("board-activity", &dump);
    }

    #[test]
    fn board_an_event_tail_matches_its_golden() {
        let (dump, _, _) = dump_frame(96, 16, &event_state(), None);
        assert_matches_golden("board-events", &dump);
    }

    #[test]
    fn board_the_tab_strip_names_every_panel_and_brackets_the_live_one() {
        for view in BoardView::ALL {
            let mut state = empty_state();
            state.view = view;
            let (dump, _, _) = dump_frame(72, 6, &state, None);
            let status = dump.lines().last().unwrap_or_default();
            for other in BoardView::ALL {
                assert!(
                    status.contains(other.as_str()),
                    "{view:?}'s status bar hides {other:?}:\n{status}"
                );
            }
            assert!(
                status.contains(&format!("[{}]", view.as_str())),
                "{view:?} is not the bracketed one:\n{status}"
            );
        }
    }

    #[test]
    fn board_a_workday_dag_matches_its_golden() {
        let state = workday_state();
        let selected = BoardTarget::Attempt(AttemptId::new("at-01"));
        let (dump, _, _) = dump_frame(72, 18, &state, Some(&selected));
        assert_matches_golden("board-dag", &dump);
    }

    #[test]
    fn board_a_workday_flow_matches_its_golden() {
        let mut state = workday_state();
        state.view = BoardView::Flow;
        let (dump, _, _) = dump_frame(72, 18, &state, None);
        assert_matches_golden("board-flow", &dump);
    }

    #[test]
    fn board_the_dag_layers_child_under_parent_with_the_state_word() {
        let state = workday_state();
        let (dump, _, _) = dump_frame(72, 18, &state, None);
        let line_of = |needle: &str| {
            dump.lines()
                .position(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("{needle:?} not rendered:\n{dump}"))
        };
        let task = line_of("harden the auth path  working");
        let lead = line_of("codex  running  (implementer)");
        let recon = line_of("recon  completed  (subagent)");
        let lint = line_of("lint  registered  (subagent)");
        let second = line_of("claude  failed");
        assert!(task < lead && lead < recon && recon < lint && lint < second);
        // One indent step per layer: the spawn sits deeper than the attempt,
        // and the chained spawn deeper still.
        let indent = |line: usize| {
            dump.lines()
                .nth(line)
                .map(|l| l.len() - l.trim_start().len())
                .unwrap_or_default()
        };
        assert!(indent(task) < indent(lead));
        assert!(indent(lead) < indent(recon));
        assert!(indent(recon) < indent(lint));
    }

    #[test]
    fn board_an_orphan_renders_under_the_off_page_header_not_silently() {
        let mut state = workday_state();
        state
            .attempts
            .iter_mut()
            .find(|attempt| attempt.id.as_str() == "at-99")
            .expect("seeded orphan")
            .engine = EngineId::new("orphan-engine");
        let (dump, _, _) = dump_frame(72, 18, &state, None);
        let header_at = dump
            .find("off-page -- parents beyond this page")
            .expect("the off-page header renders");
        // The orphan attempt renders after the header, as an attempt row.
        let orphan_at = dump
            .find("orphan-engine  running")
            .expect("the orphan renders");
        assert!(
            orphan_at > header_at,
            "the orphan lives under the header:\n{dump}"
        );

        // Without an orphan there is no header: absence sections do not
        // render for the sake of it.
        let mut clean = state;
        clean.attempts.retain(|a| a.id.as_str() != "at-99");
        let (dump, _, _) = dump_frame(72, 18, &clean, None);
        assert!(!dump.contains("off-page"), "no orphans, no header:\n{dump}");
    }

    #[test]
    fn board_a_parent_cycle_is_counted_never_followed() {
        let mut state = empty_state();
        state.tasks = vec![task(
            "t-1",
            "the only task",
            TaskState::Working,
            "2026-08-06T10:00:00Z",
        )];
        state.attempts = vec![attempt(
            "at-1",
            "t-1",
            "codex",
            AttemptState::Running,
            "2026-08-06T10:00:00Z",
        )];
        // Two spawns claiming each other as parent: no root exists, so no
        // walk reaches them. The frame must terminate and say so.
        state.nodes = vec![
            node("d-a", Some("at-1"), Some("d-b"), "left", "registered"),
            node("d-b", Some("at-1"), Some("d-a"), "right", "registered"),
        ];

        let (dump, _, _) = dump_frame(72, 12, &state, None);
        assert!(
            dump.contains("+2 unplaced -- parent cycle"),
            "a cycle is counted in words, not silently dropped:\n{dump}"
        );
        assert!(
            !dump.contains("left") && !dump.contains("right"),
            "cycle members have no honest place in the layered walk:\n{dump}"
        );
    }

    #[test]
    fn board_replies_thread_under_their_parent_and_a_lost_parent_roots() {
        let mut state = workday_state();
        state.view = BoardView::Flow;
        let (dump, _, _) = dump_frame(72, 18, &state, None);
        let brief = dump
            .lines()
            .position(|l| l.contains("orchestrator -> researcher  brief"))
            .expect("the root renders");
        let findings = dump
            .lines()
            .position(|l| l.contains("researcher -> orchestrator  findings"))
            .expect("the reply renders");
        assert!(brief < findings, "the reply sits under its parent");
        let indent = |line: usize| {
            dump.lines()
                .nth(line)
                .map(|l| l.len() - l.trim_start().len())
                .unwrap_or_default()
        };
        assert!(indent(brief) < indent(findings));

        // A reply whose parent is beyond the page roots its own thread
        // rather than vanishing with it.
        let mut lost = workday_state();
        lost.view = BoardView::Flow;
        lost.messages.retain(|m| m.id.as_str() != "m-1");
        let (dump, _, _) = dump_frame(72, 18, &lost, None);
        assert!(
            dump.contains("researcher -> orchestrator  findings"),
            "a lost parent must not take the reply with it:\n{dump}"
        );
    }

    #[test]
    fn board_reply_cycles_are_counted_in_words() {
        let mut left = message(
            "m-left",
            "orchestrator",
            "reviewer",
            "review",
            MessageState::Delivered,
            "2026-08-06T10:00:00Z",
            "2026-08-06T10:01:00Z",
        );
        let mut right = message(
            "m-right",
            "reviewer",
            "orchestrator",
            "findings",
            MessageState::Delivered,
            "2026-08-06T10:02:00Z",
            "2026-08-06T10:03:00Z",
        );
        left.reply_to = Some(MessageId::new("m-right"));
        right.reply_to = Some(MessageId::new("m-left"));
        let mut state = empty_state();
        state.view = BoardView::Flow;
        state.messages = vec![left, right];

        let (dump, hits, _) = dump_frame(72, 12, &state, None);
        assert!(
            dump.contains("+2 unthreaded -- reply cycle"),
            "a reply cycle is counted instead of disappearing:\n{dump}"
        );
        assert_eq!(hits.targets().count(), 0, "cycle rows are not actionable");
    }

    #[test]
    fn board_duplicate_ids_render_once_and_are_counted() {
        let mut state = empty_state();
        state.tasks = vec![
            task(
                "t-1",
                "first task",
                TaskState::Working,
                "2026-08-06T10:00:00Z",
            ),
            task(
                "t-1",
                "duplicate task",
                TaskState::Working,
                "2026-08-06T10:00:00Z",
            ),
        ];
        state.attempts = vec![
            attempt(
                "at-1",
                "t-1",
                "codex",
                AttemptState::Running,
                "2026-08-06T10:00:00Z",
            ),
            attempt(
                "at-1",
                "t-1",
                "duplicate-engine",
                AttemptState::Running,
                "2026-08-06T10:00:00Z",
            ),
        ];
        state.nodes = vec![
            node("d-1", Some("at-1"), None, "first spawn", "registered"),
            node("d-1", Some("at-1"), None, "duplicate spawn", "registered"),
        ];

        let (dump, hits, _) = dump_frame(72, 14, &state, None);
        assert!(dump.contains("+3 duplicate ids -- invalid page"), "{dump}");
        assert_eq!(dump.matches("first task").count(), 1, "{dump}");
        assert_eq!(dump.matches("codex  running").count(), 1, "{dump}");
        assert_eq!(dump.matches("first spawn").count(), 1, "{dump}");
        assert!(!dump.contains("duplicate task"), "{dump}");
        assert!(!dump.contains("duplicate-engine"), "{dump}");
        assert!(!dump.contains("duplicate spawn"), "{dump}");
        assert_eq!(hits.targets().count(), 3, "one target per unique id");

        let mut flow = empty_state();
        flow.view = BoardView::Flow;
        flow.messages = vec![
            message(
                "m-1",
                "first",
                "receiver",
                "brief",
                MessageState::Delivered,
                "2026-08-06T10:00:00Z",
                "2026-08-06T10:00:00Z",
            ),
            message(
                "m-1",
                "duplicate",
                "receiver",
                "brief",
                MessageState::Delivered,
                "2026-08-06T10:00:00Z",
                "2026-08-06T10:00:00Z",
            ),
        ];
        let (dump, hits, _) = dump_frame(72, 10, &flow, None);
        assert!(dump.contains("+1 duplicate id -- invalid page"), "{dump}");
        assert_eq!(dump.matches("first -> receiver").count(), 1, "{dump}");
        assert!(!dump.contains("duplicate -> receiver"), "{dump}");
        assert_eq!(hits.targets().count(), 1, "one target per unique id");
    }

    #[test]
    fn board_selection_opens_the_detail_pane_with_the_full_fields() {
        let state = workday_state();
        let selected = BoardTarget::Attempt(AttemptId::new("at-01"));
        let (dump, _, _) = dump_frame(72, 18, &state, Some(&selected));
        assert!(
            dump.contains("attempt at-01  task t-auth"),
            "the pane names the full ids the row truncates:\n{dump}"
        );
        assert!(
            dump.contains("engine codex  role implementer"),
            "the pane carries the route fields:\n{dump}"
        );
        assert!(
            dump.contains("created 2026-08-06T09:40:00Z  updated 2026-08-06T10:12:00Z"),
            "the pane carries the full stamps:\n{dump}"
        );

        // No selection, no pane; and a selection that is not in this state
        // gets no fabricated pane either.
        let (dump, _, _) = dump_frame(72, 18, &state, None);
        assert!(
            !dump.contains("attempt at-01  task"),
            "no selection, no pane"
        );
        let stale = BoardTarget::Attempt(AttemptId::new("at-gone"));
        let (dump, _, _) = dump_frame(72, 18, &state, Some(&stale));
        assert!(
            !dump.contains("attempt at-gone"),
            "a stale selection gets no pane:\n{dump}"
        );
    }

    #[test]
    fn board_every_target_kind_has_an_honest_detail_pane() {
        let state = workday_state();
        for (target, expected) in [
            (
                BoardTarget::Task(TaskId::new("t-auth")),
                &["task t-auth", "harden the auth path"][..],
            ),
            (
                BoardTarget::Node(DispatchNodeId::new("d-1")),
                &[
                    "spawn d-1",
                    "kind subagent  state completed  label recon",
                    "attempt at-01  parent absent",
                ][..],
            ),
            (
                BoardTarget::Message(MessageId::new("m-1")),
                &[
                    "message m-1  correlation c-42",
                    "orchestrator -> researcher  kind brief",
                ][..],
            ),
        ] {
            let (dump, _, _) = dump_frame(96, 18, &state, Some(&target));
            assert!(
                expected.iter().all(|line| dump.contains(line)),
                "detail for {target:?} omitted facts:\n{dump}"
            );
        }

        let mut unanchored = workday_state();
        unanchored.nodes[0].attempt_id = None;
        let target = BoardTarget::Node(DispatchNodeId::new("d-1"));
        let (dump, _, _) = dump_frame(96, 18, &unanchored, Some(&target));
        assert!(
            dump.contains("anchor absent -- no attempt or parent"),
            "missing anchors are words, not a blank line:\n{dump}"
        );
    }

    #[test]
    fn board_detail_keeps_the_selected_row_reachable_and_names_its_cut() {
        let state = workday_state();
        let order = target_order(&state);
        assert!(
            order.contains(&BoardTarget::Attempt(AttemptId::new("at-99"))),
            "keyboard order includes targets below the painted viewport"
        );
        let selected = BoardTarget::Attempt(AttemptId::new("at-01"));
        let (dump, hits, _) = dump_frame(72, 9, &state, Some(&selected));
        assert!(
            hits.targets().any(|target| target == &selected),
            "opening the pane must not cut its selected row:\n{dump}"
        );
        assert!(
            dump.contains("before") && dump.contains("more"),
            "the selected viewport names both sides of its cut:\n{dump}"
        );

        let selected = BoardTarget::Attempt(AttemptId::new("at-99"));
        let (dump, hits, _) = dump_frame(72, 14, &state, Some(&selected));
        assert!(
            hits.targets().any(|target| target == &selected),
            "an off-page selected row remains in the keyboard walk:\n{dump}"
        );

        let (dump, hits, _) = dump_frame(72, 8, &state, Some(&selected));
        assert!(
            hits.targets().any(|target| target == &selected),
            "a selected row below the initial viewport is brought into view:\n{dump}"
        );
        assert!(
            dump.contains("before"),
            "a shifted viewport names the rows cut above it:\n{dump}"
        );
    }

    #[test]
    fn board_detail_wraps_long_values_within_its_bounded_pane() {
        let mut state = empty_state();
        state.tasks = vec![task(
            "t-1",
            "a deliberately long title that wraps across the narrow detail pane",
            TaskState::Working,
            "2026-08-06T10:00:00Z",
        )];
        let selected = BoardTarget::Task(TaskId::new("t-1"));

        let (dump, _, _) = dump_frame(32, 18, &state, Some(&selected));
        assert!(dump.contains("a deliberately long title"), "{dump}");
        assert!(dump.contains("narrow detail"), "{dump}");
        assert!(dump.lines().any(|line| line.trim() == "pane"), "{dump}");
    }

    #[test]
    fn board_conflicting_dispatch_anchors_stay_with_their_declared_attempt() {
        let mut state = empty_state();
        state.tasks = vec![
            task("t-1", "first", TaskState::Working, "2026-08-06T10:00:00Z"),
            task("t-2", "second", TaskState::Working, "2026-08-06T10:00:00Z"),
        ];
        state.attempts = vec![
            attempt(
                "at-1",
                "t-1",
                "one",
                AttemptState::Running,
                "2026-08-06T10:00:00Z",
            ),
            attempt(
                "at-2",
                "t-2",
                "two",
                AttemptState::Running,
                "2026-08-06T10:00:00Z",
            ),
        ];
        state.nodes = vec![
            node("d-parent", Some("at-1"), None, "parent-one", "registered"),
            node(
                "d-child",
                Some("at-2"),
                Some("d-parent"),
                "child-two",
                "registered",
            ),
        ];

        let (dump, _, _) = dump_frame(96, 16, &state, None);
        let attempt_two = dump.find("two  running").expect("second attempt renders");
        let child = dump.find("child-two").expect("conflicting child renders");
        assert!(
            child > attempt_two,
            "the child stays with attempt at-2:\n{dump}"
        );
        assert!(
            dump.contains("+1 conflicting anchor -- invalid page"),
            "the malformed edge is named:\n{dump}"
        );
    }

    #[test]
    fn board_invalid_page_diagnostics_survive_body_overflow() {
        let mut state = workday_state();
        state.tasks.push(state.tasks[0].clone());

        let (dump, _, _) = dump_frame(72, 3, &state, None);
        assert!(
            dump.contains("+1 duplicate id -- invalid page"),
            "corruption diagnostics stay above the ordinary rows:\n{dump}"
        );

        let selected = BoardTarget::Attempt(AttemptId::new("at-99"));
        let (dump, _, _) = dump_frame(72, 8, &state, Some(&selected));
        assert!(
            dump.contains("+1 duplicate id -- invalid page"),
            "selection windowing cannot hide corruption diagnostics:\n{dump}"
        );

        state.nodes.extend([
            node(
                "d-cycle-a",
                Some("at-01"),
                Some("d-cycle-b"),
                "cycle-a",
                "registered",
            ),
            node(
                "d-cycle-b",
                Some("at-01"),
                Some("d-cycle-a"),
                "cycle-b",
                "registered",
            ),
            node(
                "d-cross",
                Some("at-99"),
                Some("d-1"),
                "cross-attempt",
                "registered",
            ),
        ]);
        let (dump, _, _) = dump_frame(72, 18, &state, None);
        for finding in [
            "+2 unplaced -- parent cycle",
            "+1 conflicting anchor -- invalid page",
            "+1 duplicate id -- invalid page",
        ] {
            assert!(
                dump.contains(finding),
                "every corruption class gets its own pinned row:\n{dump}"
            );
        }
    }

    #[test]
    fn board_unsafe_wire_glyphs_are_escaped_before_paint() {
        let mut state = empty_state();
        state.tasks = vec![task(
            "t-1",
            "ship ◆ 你好 ⚠",
            TaskState::Working,
            "2026-08-06T10:00:00Z",
        )];

        let (dump, _, _) = dump_frame(96, 8, &state, None);
        for unsafe_glyph in ['◆', '你', '好', '⚠'] {
            assert!(
                !dump.contains(unsafe_glyph),
                "unsafe glyph {unsafe_glyph:?} reached the buffer:\n{dump}"
            );
        }
        assert!(
            dump.contains("\\u{25C6}"),
            "the value stays retypable:\n{dump}"
        );
        assert!(
            dump.contains("\\u{4F60}"),
            "the value stays retypable:\n{dump}"
        );
    }

    #[test]
    fn board_selection_layers_the_foreground_and_the_mark_keeps_its_colour() {
        let mut state = empty_state();
        state.tasks = vec![task(
            "t-1",
            "the row",
            TaskState::Working,
            "2026-08-06T10:00:00Z",
        )];
        let target = BoardTarget::Task(TaskId::new("t-1"));

        let (_, _, buf) = dump_frame_tier(72, 12, &state, Some(&target), ColorTier::Truecolor);
        let running_fg = theme::state_style(theme::binding("running"), ColorTier::Truecolor).fg;
        let selection_fg = gwk_theme::SIGNAL
            .iter()
            .find(|t| t.name == "selection")
            .and_then(|t| theme::token_style(t, ColorTier::Truecolor).fg);
        assert!(
            running_fg.is_some() && selection_fg.is_some(),
            "tier emits fg"
        );
        // The task row sits at y=1 under the summary: accent 0, mark 1, text 3.
        let accent = &buf[(0, 1)];
        assert_eq!(accent.symbol(), ">");
        assert!(accent.style().add_modifier.contains(Modifier::BOLD));
        assert_eq!(
            buf[(1, 1)].style().fg,
            running_fg,
            "the mark keeps its state"
        );
        assert_eq!(
            buf[(3, 1)].style().fg,
            selection_fg,
            "the text takes the selection foreground"
        );
    }

    #[test]
    fn board_every_cell_glyph_is_ascii_or_an_admitted_mark() {
        // The admission rule (taste-gate item 5, a hard lock): nothing
        // reaches the cell buffer except ASCII and the probed MARKS
        // admission predicate — walked over the busiest frame of BOTH views, detail
        // pane open, so a stray em dash in any row or pane line fails here
        // rather than on an operator's screen.
        let state = workday_state();
        let selected = BoardTarget::Attempt(AttemptId::new("at-01"));
        let (dag, _, _) = dump_frame(72, 18, &state, Some(&selected));
        let mut flow_state = workday_state();
        flow_state.view = BoardView::Flow;
        let (flow, _, _) = dump_frame(72, 18, &flow_state, None);
        for ch in dag.chars().chain(flow.chars()) {
            assert!(
                ch.is_ascii() || gwk_theme::marks::is_admissible(ch),
                "unadmitted glyph {ch:?} in the cell buffer"
            );
        }
    }

    #[test]
    fn board_a_narrow_area_stays_inside_its_rect() {
        let state = workday_state();
        let target = BoardTarget::Task(TaskId::new("t-auth"));
        let mut hits = HitMap::new();

        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 12));
        render(
            Rect::new(0, 0, 2, 12),
            &mut buf,
            &state,
            Some(&target),
            ColorTier::Mono,
            GlyphSet::Unicode,
            &mut hits,
        );
        for y in 0..12 {
            for x in 2..40 {
                assert_eq!(
                    buf[(x, y)].symbol(),
                    " ",
                    "painted outside its area at ({x},{y})"
                );
            }
        }

        let mut empty_buf = Buffer::empty(Rect::new(0, 0, 40, 12));
        render(
            Rect::new(0, 0, 0, 12),
            &mut empty_buf,
            &state,
            Some(&target),
            ColorTier::Mono,
            GlyphSet::Unicode,
            &mut hits,
        );
        assert_eq!(empty_buf, Buffer::empty(Rect::new(0, 0, 40, 12)));
    }

    #[test]
    fn board_every_geometry_stays_inside_its_rect() {
        let mut dag = workday_state();
        dag.tasks[0].title = Some("hostile ◆ 你好 ⚠".into());
        let mut flow = dag.clone();
        flow.view = BoardView::Flow;
        let selections = [
            None,
            Some(BoardTarget::Task(TaskId::new("t-auth"))),
            Some(BoardTarget::Attempt(AttemptId::new("at-99"))),
            Some(BoardTarget::Message(MessageId::new("m-1"))),
        ];

        for state in [&dag, &flow] {
            for (origin_x, origin_y) in [(0, 0), (3, 2), (7, 5)] {
                for width in 0..=24 {
                    for height in 0..=12 {
                        for selected in &selections {
                            let outer = Rect::new(0, 0, 40, 20);
                            let area = Rect::new(origin_x, origin_y, width, height);
                            let mut buf = Buffer::empty(outer);
                            let mut hits = HitMap::new();
                            render(
                                area,
                                &mut buf,
                                state,
                                selected.as_ref(),
                                ColorTier::Mono,
                                GlyphSet::Unicode,
                                &mut hits,
                            );
                            for y in 0..outer.height {
                                for x in 0..outer.width {
                                    let inside = x >= area.x
                                        && x < area.x.saturating_add(area.width)
                                        && y >= area.y
                                        && y < area.y.saturating_add(area.height);
                                    if !inside {
                                        assert_eq!(
                                            buf[(x, y)].symbol(),
                                            " ",
                                            "paint escaped {area:?} at ({x},{y})"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn board_spawn_indent_caps_visually_and_labels_the_real_depth() {
        let mut state = empty_state();
        state.tasks = vec![task(
            "t-1",
            "deep tree",
            TaskState::Working,
            "2026-08-06T10:00:00Z",
        )];
        state.attempts = vec![attempt(
            "at-1",
            "t-1",
            "codex",
            AttemptState::Running,
            "2026-08-06T10:00:00Z",
        )];
        state.nodes = (0..12)
            .map(|index| {
                node(
                    &format!("d-{index:02}"),
                    Some("at-1"),
                    (index > 0)
                        .then(|| format!("d-{:02}", index - 1))
                        .as_deref(),
                    &format!("layer{index}"),
                    "registered",
                )
            })
            .collect();

        let (dump, _, _) = dump_frame(96, 20, &state, None);
        let leading = |label: &str| {
            let line = dump
                .lines()
                .find(|line| line.contains(label))
                .unwrap_or_else(|| panic!("{label} missing:\n{dump}"));
            line.len() - line.trim_start().len()
        };
        assert!(leading("layer3") < leading("layer4"), "{dump}");
        assert_eq!(leading("layer4"), leading("layer11"), "{dump}");
        assert!(dump.contains("+7 "), "{dump}");
        assert!(dump.contains("+13 "), "{dump}");
    }

    #[test]
    fn board_overflow_names_the_cut_and_cut_rows_take_no_hits() {
        let state = workday_state();
        let (dump, hits, _) = dump_frame(72, 6, &state, None);
        assert!(
            dump.contains("+7 more"),
            "an overflowing body names the cut:\n{dump}"
        );
        // 11 built rows, 4 painted (5 body rows, one spent on the notice):
        // the walk sees exactly what the frame shows.
        assert_eq!(
            hits.targets().count(),
            3,
            "targets stop at the cut:\n{dump}"
        );
    }

    #[test]
    fn board_clicks_land_on_rows_and_the_keyboard_walks_the_same_targets() {
        let state = workday_state();
        let (dump, hits, _) = dump_frame(72, 18, &state, None);
        // The summary line is not actionable; the first task row is.
        assert_eq!(hits.hit(5, 0), None, "the summary is not a target:\n{dump}");
        assert_eq!(
            hits.hit(5, 1),
            Some(&BoardTarget::Task(TaskId::new("t-auth"))),
            "the task row is a click target:\n{dump}"
        );
        // Every painted actionable row is in the keyboard walk, in paint
        // order: 3 tasks + 4 attempts + 2 spawns.
        let walked: Vec<&BoardTarget> = hits.targets().collect();
        assert_eq!(walked.len(), 9, "every actionable row walks:\n{dump}");
        assert_eq!(walked[0], &BoardTarget::Task(TaskId::new("t-auth")));
        assert_eq!(walked[1], &BoardTarget::Attempt(AttemptId::new("at-01")));

        let mut flow = workday_state();
        flow.view = BoardView::Flow;
        let (dump, hits, _) = dump_frame(72, 18, &flow, None);
        let walked: Vec<&BoardTarget> = hits.targets().collect();
        assert_eq!(walked.len(), 4, "every message row walks:\n{dump}");
        assert!(
            walked
                .iter()
                .all(|target| matches!(target, BoardTarget::Message(_))),
            "the flow walk contains message targets only:\n{dump}"
        );
    }

    #[test]
    fn board_the_status_bar_names_the_active_view_and_the_running_count() {
        let state = workday_state();
        let (dump, _, _) = dump_frame(72, 18, &state, None);
        assert_eq!(
            dump.lines().nth(17),
            Some("BOARD estate brief run [dag] flow events replay fleet cost audit  r2@407"),
            "the bar names the view, the pulse, and the page:\n{dump}"
        );
        assert!(
            dump.contains("3 tasks  2 running"),
            "the summary agrees with the bar:\n{dump}"
        );

        let mut flow = workday_state();
        flow.view = BoardView::Flow;
        let (dump, _, _) = dump_frame(72, 18, &flow, None);
        assert_eq!(
            dump.lines().nth(17),
            Some("BOARD estate brief run dag [flow] events replay fleet cost audit  r2@407"),
            "the flow view keeps the ambient pulse:\n{dump}"
        );
    }

    #[test]
    fn embedded_board_status_names_the_five_lens_surface() {
        let state = workday_state();
        let mut terminal = Terminal::new(TestBackend::new(72, 18)).expect("terminal");
        let mut hits = HitMap::new();
        terminal
            .draw(|frame| {
                render_embedded(
                    frame.area(),
                    frame.buffer_mut(),
                    &state,
                    None,
                    ColorTier::Mono,
                    GlyphSet::Unicode,
                    &mut hits,
                );
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let footer = (0..buffer.area.width)
            .map(|x| buffer[(x, buffer.area.height - 1)].symbol())
            .collect::<String>();
        assert_eq!(footer.trim_end(), "TASKS  r2@407");
        assert!(!footer.contains("BOARD"));
    }
}
