//! The Board lens — the two work views of the demo cut.
//!
//! The task/attempt DAG and the A2A message flow, and nothing else: the
//! other Board panels — fleet, cost/health, evidence, brain — are out of
//! this phase by ruling, not omission. Both views are row surfaces: a Board
//! row has room for words, so the state is printed as a word and the mark
//! cell carries the graph tier — WHAT KIND of node the row is — from the
//! same admitted inventory as every other glyph.
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
//! This work view carries budget caps (`Attempt::budget`) but does not join the
//! separately projected `CostEntry` usage rows owned by the cut cost/health
//! panel. A gauge here would therefore fabricate a local numerator. When that
//! panel lands, the ruled form is braille — one width class and font risk.
//!
//! # Absent parents are said in words
//!
//! Pagination can hand this lens an attempt whose task is beyond the page,
//! or a spawn whose anchor is. Those rows render under an `off-page` header
//! rather than vanishing, and a parent cycle in the wire data is counted,
//! never followed: absence is a fact, and facts get words.

use gwk_domain::entity::{Attempt, DispatchNode, Message, Task};
use gwk_domain::fsm::{AttemptState, MessageState, TaskState};
use gwk_domain::ids::{AttemptId, DispatchNodeId, MessageId, Seq, TaskId, Timestamp};
use gwk_theme::marks::{GlyphSet, Mark, StateBinding};
use gwk_theme::tier::ColorTier;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use crate::input::HitMap;
use crate::theme;

/// Which work view the frame shows. One lens, two views, one visible at a
/// time — the other is a keystroke away, never a second pane fighting for
/// the same columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BoardView {
    #[default]
    Dag,
    Flow,
}

/// Everything the lens paints, assembled by the caller from projection
/// pages. The lens never fetches: it is a pure function of this value.
#[derive(Debug, Clone)]
pub struct BoardState {
    pub view: BoardView,
    pub tasks: Vec<Task>,
    pub attempts: Vec<Attempt>,
    pub nodes: Vec<DispatchNode>,
    pub messages: Vec<Message>,
    /// The projector's as-of stamp from the page this state was read at.
    pub watermark: Option<Seq>,
}

/// What a Board row stands for — the target a click or a keystroke acts on.
/// Every target opens the detail pane; the Board has no mutating verbs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoardTarget {
    Task(TaskId),
    Attempt(AttemptId),
    Node(DispatchNodeId),
    Message(MessageId),
}

/// A spawn chain deeper than this stops indenting further: the tree is
/// still walked and every node still gets a row, but a pathological chain
/// cannot push its own text off the pane.
const INDENT_CAP: u16 = 6;

/// Cells per layer of the DAG.
const INDENT_STEP: u16 = 2;

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
    /// The DAG layer, in indent cells before the mark.
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
        indent: indent.min(INDENT_CAP * INDENT_STEP),
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
                    stack.push((
                        reply,
                        indent
                            .saturating_add(INDENT_STEP)
                            .min(INDENT_CAP * INDENT_STEP),
                    ));
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

fn rows(state: &BoardState, tier: ColorTier) -> Vec<Row> {
    match state.view {
        BoardView::Dag => dag_rows(state, tier),
        BoardView::Flow => flow_rows(state, tier),
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
        BoardTarget::Attempt(id) => {
            let attempt = state
                .attempts
                .iter()
                .find(|a| a.id.as_str() == id.as_str())?;
            let (word, _) = attempt_face(attempt.state);
            Some(vec![
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
                format!(
                    "created {}  updated {}",
                    attempt.created_at.as_str(),
                    attempt.updated_at.as_str()
                ),
            ])
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

    let pinned = diagnostic_rows.min(body_rows as usize);
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
        let text = theme::safe_text(&row.text, area.width.saturating_sub(1) as usize);
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
                " ",
                Style::default().add_modifier(Modifier::REVERSED),
            );
        }
        let mut x = area.x + 1 + row.indent.min(INDENT_CAP * INDENT_STEP);
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
            let safe_right = theme::safe_text(right, area.width as usize);
            let right_width = UnicodeWidthStr::width(safe_right.as_ref());
            let text_width = UnicodeWidthStr::width(safe_text.as_ref());
            let rx = (area.x + area.width)
                .saturating_sub(u16::try_from(right_width).unwrap_or(u16::MAX));
            if rx > x.saturating_add(u16::try_from(text_width).unwrap_or(u16::MAX)) {
                buf.set_string(rx, y, safe_right.as_ref(), text_style);
            }
        }
        if let Some(target) = &row.target {
            hits.register(Rect::new(area.x, y, area.width, 1), target.clone());
        }
    }

    if overflow > 0 {
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
    // as-of stamp. The inactive view's name stays visible — the other half
    // of the demo cut is one keystroke away, and the bar says so.
    let tabs = match state.view {
        BoardView::Dag => "[dag] flow",
        BoardView::Flow => "dag [flow]",
    };
    let as_of = state
        .watermark
        .as_ref()
        .map_or_else(|| "-".to_string(), |w| w.to_string());
    let status = format!(
        "BOARD  {tabs}  {} running  as-of {as_of}",
        running_count(state)
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
    use gwk_domain::ids::{CorrelationId, EngineId, IdempotencyKey};
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

    fn empty_state() -> BoardState {
        BoardState {
            view: BoardView::Dag,
            tasks: Vec::new(),
            attempts: Vec::new(),
            nodes: Vec::new(),
            messages: Vec::new(),
            watermark: None,
        }
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
            attempts: vec![lead, second, shipped, orphan],
            nodes: vec![
                node("d-1", Some("at-01"), None, "recon", "completed"),
                node("d-2", Some("at-01"), Some("d-1"), "lint", "registered"),
            ],
            messages: vec![brief, findings, dead, pending],
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
            dump.contains("as-of -"),
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
        assert_eq!(accent.symbol(), " ");
        assert!(accent.style().add_modifier.contains(Modifier::REVERSED));
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
    fn board_spawn_indent_stops_at_the_cap() {
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
        let column = |label: &str| {
            dump.lines()
                .find_map(|line| line.find(label))
                .unwrap_or_else(|| panic!("{label} missing:\n{dump}"))
        };
        assert!(column("layer3") < column("layer4"), "{dump}");
        assert_eq!(column("layer4"), column("layer11"), "{dump}");
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
            Some("BOARD  [dag] flow  2 running  as-of 407"),
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
            Some("BOARD  dag [flow]  2 running  as-of 407"),
            "the flow view keeps the ambient pulse:\n{dump}"
        );
    }
}
