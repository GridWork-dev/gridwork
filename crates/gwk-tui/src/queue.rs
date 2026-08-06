//! The Queue lens — the workday screen.
//!
//! One ranked list of everything asking for the operator: open gates, raised
//! attention, arriving messages, and a recent section for what was closed.
//! The loudest element states the all-clear (the verdict line at row zero),
//! resolved items demote to `recent` rather than gray in place, and absence
//! renders as words, never blank cells.
//!
//! # Gates land here, and nowhere else
//!
//! A relayed permission prompt renders as Queue rows plus the status-bar
//! count. Nothing ever seizes the focused pane: a TUI has no scrim, and the
//! accepted tradeoff is that a genuinely urgent gate waits one keystroke
//! away. A gate that is already decided renders as a **typed result** —
//! the kernel has no gate expiry and `decide_gate` applies whether or not
//! anything still listens, so a prompt shown as answerable after its
//! decision would be a stale prompt over a dead attempt.
//!
//! # Rank
//!
//! `priority` ascending — 0 first, the P0 idiom — with unranked after ranked
//! and byte-ordered ids breaking ties, which is the same order the wire's
//! `COLLATE "C"` pagination walks (opx-01). Rank is presentation only:
//! cost-of-waiting is explicitly deferred by the contract, and this lens does
//! not fake it client-side.
//!
//! # Quiet is not closed
//!
//! An acked or muted row stays on screen with its stamp said in words, and
//! neither counts toward the verdict line or the status bar — quieting is
//! one act with two halves, so the count and the `!` marks always agree.
//! Both leave `resolved_at` absent — the item still holds its dedup slot —
//! and only resolve re-arms: the `recent` header carries that sentence
//! because the demotion is exactly where an operator would otherwise assume
//! the problem cannot come back.

use gwk_domain::command::KernelCommand;
use gwk_domain::entity::{AttentionItem, Gate, Message, Receipt};
use gwk_domain::fsm::{GateVerdict, MessageState};
use gwk_domain::ids::{AttentionItemId, GateId, Seq, Timestamp};
use gwk_theme::marks::{GlyphSet, StateBinding};
use gwk_theme::tier::ColorTier;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use crate::input::HitMap;
use crate::theme;
use crate::theme::binding;

/// Everything the lens paints, assembled by the caller from projection pages.
///
/// The lens never fetches: it is a pure function of this value, which is what
/// makes every frame below testable without a kernel.
#[derive(Debug, Clone)]
pub struct QueueState {
    pub attention: Vec<AttentionItem>,
    /// Prompt gates only reach rows (`question` present); the rest of the
    /// gate table belongs to other surfaces.
    pub gates: Vec<Gate>,
    /// The authority audit trail, for the occurrence count on `authority`
    /// rows. Receipts are the ledger of every page; the attention item dedups.
    pub receipts: Vec<Receipt>,
    /// Arrived inter-party messages, newest first. Their presence in this
    /// list IS the non-motion representation of arrival: with motion off (or
    /// absent entirely) the fact still has a row.
    pub messages: Vec<Message>,
    /// The projector's as-of stamp from the page this state was read at.
    pub watermark: Option<Seq>,
    /// The clock the caller read when assembling the state. Mute deadlines
    /// compare against this, never against a clock the render reads itself —
    /// a frame is a value, and a value does not tell time.
    ///
    /// Must be the kernel's own stamp form: UTC, `Z`-suffixed RFC 3339, the
    /// one shape the store pins (`SET TIME ZONE 'UTC'`). `Timestamp`'s byte
    /// order is instant order only within that single form — a local-offset
    /// stamp here would shift every mute comparison by the offset.
    pub now: Timestamp,
}

/// What a Queue row stands for — the target a click or a keystroke acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueTarget {
    Attention(AttentionItemId),
    Gate(GateId),
}

/// How many message rows the mail section shows. Arrival is the fact being
/// represented; history is the drill-down's job.
const MAIL_BUDGET: usize = 3;

/// Stamp the selected attention item seen.
pub fn ack(target: &QueueTarget) -> Option<KernelCommand> {
    match target {
        QueueTarget::Attention(id) => Some(KernelCommand::AckAttention {
            attention_item_id: id.clone(),
        }),
        // A gate is decided, never acked — quieting a question nobody
        // answered would be the stale prompt this lens exists to refuse.
        QueueTarget::Gate(_) => None,
    }
}

/// Quiet the selected attention item until `until`. The deadline is the
/// caller's: the lens has no clock and no opinion on how long quiet lasts.
pub fn mute(target: &QueueTarget, until: Timestamp) -> Option<KernelCommand> {
    match target {
        QueueTarget::Attention(id) => Some(KernelCommand::MuteAttention {
            attention_item_id: id.clone(),
            muted_until: until,
        }),
        QueueTarget::Gate(_) => None,
    }
}

/// Is this item asking out loud right now?
///
/// Unresolved, not acked, and not muted into the future. Ack and mute quiet
/// the row and the ambient count in one act — the verdict line, the
/// status-bar `!N`, and the set of `!` marks all derive from this predicate,
/// so the frame never states a number its own rows contradict. Both leave
/// the item open: quiet is presentation, resolve is the only close.
///
/// The comparison is byte order over the contract-pinned UTC RFC 3339 form —
/// see [`QueueState::now`] for why that is instant order.
fn audible(item: &AttentionItem, now: &Timestamp) -> bool {
    item.resolved_at.is_none()
        && item.acked_at.is_none()
        && !item.muted_until.as_ref().is_some_and(|until| until > now)
}

/// Unresolved items in presentation order: priority ascending (0 first, the
/// P0 idiom), unranked after ranked, ties and the unranked tail in byte order
/// — the order the wire's `COLLATE "C"` pagination already walks.
fn ranked(items: &[AttentionItem]) -> Vec<&AttentionItem> {
    let mut open: Vec<&AttentionItem> = items.iter().filter(|i| i.resolved_at.is_none()).collect();
    open.sort_by(|a, b| {
        match (a.priority, b.priority) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
        .then_with(|| a.id.as_str().cmp(b.id.as_str()))
    });
    open
}

/// The occurrence trail behind an `authority` row: how many receipts the
/// kernel wrote for this subject, and since when.
///
/// The attention item dedups — one row per live problem — while every page
/// writes a receipt, so the receipts are where "how often" lives. The join is
/// the kernel's own construction: a page's receipt carries the subject as
/// `subject_type`/`subject_id` and its attention item carries the same pair
/// joined as `subject_ref`.
fn occurrences<'a>(
    item: &AttentionItem,
    receipts: &'a [Receipt],
) -> Option<(usize, &'a Timestamp)> {
    if item.kind != "authority" {
        // The count renders on authority rows only (ruled); other kinds have
        // no receipt trail with this shape to count.
        return None;
    }
    let subject_ref = item.subject_ref.as_deref()?;
    let mut count = 0;
    let mut since: Option<&Timestamp> = None;
    for receipt in receipts {
        // The kernel's joined form, matched without re-joining it — no
        // allocation on the render path.
        let matched = subject_ref
            .strip_prefix(receipt.subject_type.as_str())
            .and_then(|rest| rest.strip_prefix('/'))
            == Some(receipt.subject_id.as_str());
        if matched {
            count += 1;
            since = Some(match since {
                Some(s) if s <= &receipt.ts => s,
                _ => &receipt.ts,
            });
        }
    }
    since.map(|s| (count, s))
}

/// `HH:MM` out of an RFC 3339 timestamp, for the compact times rows carry.
fn hhmm(ts: &Timestamp) -> &str {
    ts.as_str().get(11..16).unwrap_or(ts.as_str())
}

/// Is this gate an open prompt — a question still waiting on a human?
///
/// The typed verdict alone decides: `Pending` is open, anything else is
/// decided. `chosen_option` is not consulted, so contradictory wire data — a
/// chosen option beside a `Pending` verdict — stays a prompt rather than
/// rendering the self-contradicting "decided: x (pending)".
fn open_prompt(gate: &Gate) -> bool {
    gate.question.is_some() && gate.verdict == GateVerdict::Pending
}

/// The one count every loud element states: open gates plus audible
/// attention. The verdict line, the status bar, and the set of `!` marks all
/// derive from this single walk, so they cannot drift apart.
fn audible_count(state: &QueueState) -> usize {
    state.gates.iter().filter(|g| open_prompt(g)).count()
        + state
            .attention
            .iter()
            .filter(|i| audible(i, &state.now))
            .count()
}

/// One row's paint: mark cell, styled text, and what it stands for.
struct Row {
    mark: Option<&'static StateBinding>,
    text: String,
    /// A right-aligned tail, painted only when it clears the text — dropped,
    /// not squeezed, in a pane too narrow to hold both.
    right: Option<String>,
    style: Style,
    target: Option<QueueTarget>,
}

impl Row {
    fn plain(text: String, style: Style) -> Self {
        Row {
            mark: None,
            text,
            right: None,
            style,
            target: None,
        }
    }
}

/// Build the frame's rows from the state. Pure, and the whole layout: the
/// painter below only places what this returns.
fn rows(state: &QueueState, tier: ColorTier) -> Vec<Row> {
    let mut out = Vec::new();
    let muted_style = theme::state_style(binding("idle"), tier);
    let warn = binding("needs_attention");
    let done = binding("done");

    let open_gates: Vec<&Gate> = state.gates.iter().filter(|g| open_prompt(g)).collect();
    let count = audible_count(state);

    // The verdict line: the loudest element states the all-clear. Non-empty,
    // it states the count and — right-aligned — the oldest age.
    if count == 0 {
        out.push(Row::plain(
            "all clear -- nothing needs attention".into(),
            theme::state_style(done, tier),
        ));
    } else {
        let oldest = open_gates
            .iter()
            .map(|g| &g.created_at)
            .chain(
                state
                    .attention
                    .iter()
                    .filter(|i| audible(i, &state.now))
                    .map(|i| &i.raised_at),
            )
            .min();
        out.push(Row {
            mark: None,
            text: format!("{count} need attention"),
            right: oldest.map(|t| format!("oldest {}", hhmm(t))),
            style: theme::state_style(warn, tier).add_modifier(Modifier::BOLD),
            target: None,
        });
    }

    // Open gates: the question, then its options one keystroke away.
    for gate in &open_gates {
        let question = gate.question.as_deref().unwrap_or_default();
        let kind = gate.kind.as_deref().unwrap_or("gate");
        out.push(Row {
            mark: Some(warn),
            text: format!("gate {kind}  {question}"),
            right: None,
            style: theme::state_style(warn, tier),
            target: Some(QueueTarget::Gate(gate.id.clone())),
        });
        if let Some(options) = &gate.options {
            out.push(Row::plain(
                format!("   options: {}", options.join(" / ")),
                muted_style,
            ));
        }
    }

    // Attention, ranked. Quiet rows stay visible with their stamp in words:
    // acked and muted are presentation states, and hiding them would read as
    // resolved — the one thing they deliberately are not. The summary owns
    // the left edge — it is the only guaranteed human-readable text, so the
    // quiet decorations sit after it and are what a narrow pane clips first.
    for item in ranked(&state.attention) {
        let mut text = item.summary.clone();
        if let Some((count, since)) = occurrences(item, &state.receipts) {
            text.push_str(&format!("  {count}x since {}", hhmm(since)));
        }
        let live_mute = item
            .muted_until
            .as_ref()
            .filter(|until| **until > state.now);
        let (mark, style) = if live_mute.is_some() || item.acked_at.is_some() {
            (binding("idle"), muted_style)
        } else {
            (warn, theme::state_style(warn, tier))
        };
        if let Some(until) = live_mute {
            text.push_str(&format!("  muted until {}", hhmm(until)));
        } else if item.acked_at.is_some() {
            text.push_str("  acked");
        }
        out.push(Row {
            mark: Some(mark),
            text: format!("{text}  ({})", item.kind),
            right: None,
            style,
            target: Some(QueueTarget::Attention(item.id.clone())),
        });
    }

    // Mail: arrival as standing rows. This is the non-motion representation —
    // with `--motion=off` (or no motion machinery at all) an arrived message
    // is still a fact on the workday screen, not a glint that already faded.
    // Arrived means it: an accepted-but-undelivered or failed message has no
    // arrival to represent, so it gets no arrival row. The stamp is the last
    // state change — for a freshly delivered message, the delivery itself.
    let arrived: Vec<&Message> = state
        .messages
        .iter()
        .filter(|m| {
            matches!(
                m.state,
                MessageState::Delivered | MessageState::Acknowledged | MessageState::Applied
            )
        })
        .collect();
    if arrived.is_empty() {
        out.push(Row::plain("no messages".into(), muted_style));
    } else {
        for message in arrived.iter().take(MAIL_BUDGET) {
            let sender = message.sender.as_deref().unwrap_or("?");
            let recipient = message.recipient.as_deref().unwrap_or("?");
            let kind = message.kind.as_deref().unwrap_or("message");
            out.push(Row::plain(
                format!(
                    "msg {}  {sender} to {recipient}  ({kind})",
                    hhmm(&message.updated_at)
                ),
                muted_style,
            ));
        }
        if arrived.len() > MAIL_BUDGET {
            // The same honesty the body overflow keeps: a cut is named.
            out.push(Row::plain(
                format!("+{} more", arrived.len() - MAIL_BUDGET),
                muted_style,
            ));
        }
    }

    // Recent: what closed, demoted below the live rows. Resolve is the only
    // verb that frees the dedup slot, so the header says what the demotion
    // means before anyone relies on it.
    out.push(Row::plain(
        "recent -- resolve is re-arm: a recurring problem raises anew".into(),
        muted_style,
    ));
    let mut recent = 0usize;
    for gate in state
        .gates
        .iter()
        .filter(|g| !open_prompt(g) && g.question.is_some())
    {
        // The already-decided prompt as a typed result — never a question
        // still offering its options (opx-02).
        let chosen = gate
            .chosen_option
            .as_deref()
            .unwrap_or("(no option recorded)");
        let verdict = format!("{:?}", gate.verdict).to_lowercase();
        out.push(Row {
            mark: Some(done),
            text: format!("decided: {chosen} ({verdict})"),
            right: None,
            style: muted_style,
            target: None,
        });
        recent += 1;
    }
    for item in state.attention.iter().filter(|i| i.resolved_at.is_some()) {
        let resolution = item.resolution.as_deref().unwrap_or("resolved");
        out.push(Row {
            mark: Some(done),
            text: format!("{}  -- {resolution}", item.summary),
            right: None,
            style: muted_style,
            target: None,
        });
        recent += 1;
    }
    if recent == 0 {
        out.push(Row::plain("   nothing recent".into(), muted_style));
    }

    out
}

/// Paint the Queue into `area`, registering every painted actionable row in
/// `hits`.
///
/// Column zero is the accent column: the selected row carries a reverse-video
/// space there — the tier-independent selection primitive — and at the two
/// tiers that emit colour the row's text additionally takes the `selection`
/// foreground, layered over the row's own style (STM Fork B). The last row is
/// the status bar, always exactly one row (decision 39); a body that
/// overflows names the cut instead of silently truncating. Cut rows are
/// off-frame, not scrolled to: they register no hit and appear in no
/// [`HitMap::targets`] walk, so neither input path can act on a row the
/// operator cannot see.
pub fn render(
    area: Rect,
    buf: &mut Buffer,
    state: &QueueState,
    selected: Option<&QueueTarget>,
    tier: ColorTier,
    glyphs: GlyphSet,
    hits: &mut HitMap<QueueTarget>,
) {
    hits.clear();
    if area.height == 0 || area.width == 0 {
        return;
    }
    let body_rows = area.height - 1;
    let built = rows(state, tier);
    let overflow = built.len().saturating_sub(body_rows as usize);
    let visible = if overflow > 0 {
        // One of the visible rows becomes the cut notice.
        (body_rows as usize).saturating_sub(1)
    } else {
        built.len()
    };

    let selection_fg = match tier {
        // At ≥256 colours the selected row's foreground shifts to `selection`
        // on top of the accent; at 16/mono the accent cell is the entire
        // signal (STM: the reverse-video space is the carrier at every tier).
        ColorTier::Truecolor | ColorTier::Xterm256 => gwk_theme::SIGNAL
            .iter()
            .find(|t| t.name == "selection")
            .map(|t| theme::token_style(t, tier)),
        ColorTier::Ansi16 | ColorTier::Mono => None,
    };

    for (i, row) in built.iter().take(visible).enumerate() {
        let y = area.y + i as u16;
        let is_selected = matches!((selected, &row.target), (Some(sel), Some(t)) if *sel == *t);
        let text_style = match (is_selected, selection_fg) {
            // Layered, not replaced: the fg shifts to `selection`, the row's
            // own modifiers stay.
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
        let mut x = area.x + 1;
        if let Some(state_binding) = row.mark {
            let mark =
                gwk_theme::marks::mark(state_binding.mark).expect("the mark inventory is pinned");
            // Static marks only in this lens; frame 0 is every frame. The
            // mark keeps the row's binding style even on the selected row —
            // the state colour is the mark's meaning — and its write is
            // bounded by the area like every other one.
            let glyph = theme::glyph(mark, 0, glyphs);
            let budget = (area.x + area.width).saturating_sub(x);
            buf.set_stringn(x, y, glyph.to_string(), budget as usize, row.style);
            x += 2;
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
        let y = area.y + body_rows.saturating_sub(1);
        buf.set_stringn(
            area.x + 1,
            y,
            format!("+{} more", built.len() - visible),
            area.width.saturating_sub(1) as usize,
            theme::state_style(binding("idle"), tier),
        );
    }

    // The status bar: the ambient half of the gates answer. `!` is the
    // admitted needs-attention glyph — U+26A0 is banned from the cell buffer.
    let as_of = state
        .watermark
        .as_ref()
        .map_or_else(|| "-".to_string(), |w| w.to_string());
    let status = format!("QUEUE  !{}  as-of {as_of}", audible_count(state));
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
    use gwk_domain::envelope::Actor;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    const NOW: &str = "2026-08-06T12:00:00Z";

    fn ts(s: &str) -> Timestamp {
        Timestamp::new(s)
    }

    fn item(id: &str, summary: &str) -> AttentionItem {
        AttentionItem {
            id: AttentionItemId::new(id),
            kind: "watchdog".into(),
            summary: summary.into(),
            subject_ref: None,
            raised_by: None,
            priority: None,
            raised_at: ts("2026-08-06T09:00:00Z"),
            acked_at: None,
            muted_until: None,
            resolved_at: None,
            resolution: None,
        }
    }

    fn gate(id: &str, question: &str) -> Gate {
        Gate {
            id: GateId::new(id),
            version: 1,
            attempt_id: None,
            phase_ref: None,
            kind: Some("permission".into()),
            question: Some(question.into()),
            options: Some(vec!["allow".into(), "deny".into()]),
            verdict: GateVerdict::Pending,
            chosen_option: None,
            evidence_ref: None,
            created_at: ts("2026-08-06T09:30:00Z"),
            updated_at: ts("2026-08-06T09:30:00Z"),
        }
    }

    fn receipt(subject_type: &str, subject_id: &str, at: &str) -> Receipt {
        Receipt {
            id: gwk_domain::ids::ReceiptId::new(format!("r-{at}")),
            actor: Actor {
                kind: "kernel".into(),
                id: None,
            },
            action: "deploy".into(),
            subject_type: subject_type.into(),
            subject_id: subject_id.into(),
            from: None,
            to: None,
            observed_basis: None,
            ts: ts(at),
        }
    }

    fn empty_state() -> QueueState {
        QueueState {
            attention: Vec::new(),
            gates: Vec::new(),
            receipts: Vec::new(),
            messages: Vec::new(),
            watermark: None,
            now: ts(NOW),
        }
    }

    fn dump_frame(
        w: u16,
        h: u16,
        state: &QueueState,
        selected: Option<&QueueTarget>,
    ) -> (String, HitMap<QueueTarget>, Buffer) {
        dump_frame_tier(w, h, state, selected, ColorTier::Mono)
    }

    fn dump_frame_tier(
        w: u16,
        h: u16,
        state: &QueueState,
        selected: Option<&QueueTarget>,
        tier: ColorTier,
    ) -> (String, HitMap<QueueTarget>, Buffer) {
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

    fn workday_state() -> QueueState {
        let mut kek = item("a-kek", "KEK rotation is overdue");
        kek.priority = Some(0);
        let mut deploy = item("a-deploy", "deploy blocked on grant");
        deploy.kind = "authority".into();
        deploy.subject_ref = Some("command/cmd-7".into());
        deploy.priority = Some(1);
        let mut disk = item("a-disk", "disk pressure on the build host");
        disk.acked_at = Some(ts("2026-08-06T10:00:00Z"));
        let mut flap = item("a-flap", "mcp facade flapping");
        flap.muted_until = Some(ts("2026-08-06T18:00:00Z"));
        let mut fixed = item("a-fixed", "runner pool exhausted");
        fixed.resolved_at = Some(ts("2026-08-06T11:00:00Z"));
        fixed.resolution = Some("pool widened".into());

        let mut decided = gate("g-old", "push to main?");
        decided.verdict = GateVerdict::Pass;
        decided.chosen_option = Some("allow".into());

        QueueState {
            attention: vec![disk, kek, flap, deploy, fixed],
            gates: vec![gate("g-1", "rm -rf build/ in worktree?"), decided],
            receipts: vec![
                receipt("command", "cmd-7", "2026-08-06T09:14:00Z"),
                receipt("command", "cmd-7", "2026-08-06T10:20:00Z"),
                receipt("command", "cmd-7", "2026-08-06T11:41:00Z"),
                receipt("command", "other", "2026-08-06T09:00:00Z"),
            ],
            messages: vec![Message {
                id: gwk_domain::ids::MessageId::new("m-1"),
                version: 1,
                state: gwk_domain::fsm::MessageState::Delivered,
                idempotency_key: gwk_domain::ids::IdempotencyKey::new("k-1"),
                correlation_id: None,
                reply_to: None,
                sender: Some("researcher".into()),
                recipient: Some("orchestrator".into()),
                channel: None,
                kind: Some("findings".into()),
                payload: None,
                deadline: None,
                delivery_attempts: 1,
                dead_letter_reason: None,
                delivery_refs: None,
                created_at: ts("2026-08-06T11:58:00Z"),
                updated_at: ts("2026-08-06T11:58:00Z"),
            }],
            watermark: Some(Seq::new(213)),
            now: ts(NOW),
        }
    }

    #[test]
    fn queue_rank_orders_priority_first_then_byte_order_with_unranked_last() {
        let mut b = item("b", "ranked one");
        b.priority = Some(1);
        let mut a = item("a", "ranked zero late id");
        a.priority = Some(0);
        let mut z = item("z", "ranked zero");
        z.priority = Some(0);
        let unranked = item("0-first-by-id", "unranked");
        let items = [b, a, z, unranked];
        let order: Vec<&str> = ranked(&items).iter().map(|i| i.id.as_str()).collect();
        // 0 before 1 (the P0 idiom), byte order inside a rank, and an
        // unranked item last no matter how early its id sorts.
        assert_eq!(order, vec!["a", "z", "b", "0-first-by-id"]);
    }

    #[test]
    fn queue_the_all_none_frame_renders_words_not_blank() {
        let (dump, _, _) = dump_frame(72, 7, &empty_state(), None);
        assert!(
            dump.contains("all clear -- nothing needs attention"),
            "the verdict line states the all-clear:\n{dump}"
        );
        assert!(dump.contains("no messages"), "absence in words:\n{dump}");
        assert!(dump.contains("nothing recent"), "absence in words:\n{dump}");
        assert!(
            dump.contains("as-of -"),
            "an empty log's watermark is said, not skipped:\n{dump}"
        );
        assert_matches_golden("queue-empty", &dump);
    }

    #[test]
    fn queue_a_workday_frame_matches_its_golden() {
        let state = workday_state();
        let selected = QueueTarget::Gate(GateId::new("g-1"));
        let (dump, _, _) = dump_frame(72, 14, &state, Some(&selected));
        assert_matches_golden("queue-workday", &dump);
    }

    #[test]
    fn queue_a_decided_gate_renders_a_typed_result_never_a_stale_prompt() {
        // The seeded negative (opx-02): the kernel has no gate expiry, so the
        // lens is the only thing standing between a decided gate and a prompt
        // that looks answerable. A decided gate offering options fails here.
        let mut decided = gate("g-done", "grant the deploy?");
        decided.verdict = GateVerdict::Pass;
        decided.chosen_option = Some("allow".into());
        let mut state = empty_state();
        state.gates = vec![decided];

        let (dump, hits, _) = dump_frame(72, 10, &state, None);
        assert!(
            dump.contains("decided: allow (pass)"),
            "a decided gate is a typed result:\n{dump}"
        );
        assert!(
            !dump.contains("options:"),
            "a decided gate must not offer options:\n{dump}"
        );
        assert!(
            !dump.contains("gate permission  grant the deploy?"),
            "a decided gate must not render as an open prompt:\n{dump}"
        );
        assert!(
            hits.targets().next().is_none(),
            "a decided gate is not an actionable row"
        );
    }

    #[test]
    fn queue_an_open_gate_never_seizes_the_pane() {
        // P13(a): the gate is rows among rows, not a modal. Everything else
        // still paints in the same frame.
        let mut state = empty_state();
        state.gates = vec![gate("g-1", "proceed?")];
        state.attention = vec![item("a-1", "still visible")];

        let (dump, _, _) = dump_frame(72, 10, &state, None);
        assert!(dump.contains("proceed?"));
        assert!(
            dump.contains("still visible"),
            "an open gate must not displace the rest of the queue:\n{dump}"
        );
    }

    #[test]
    fn queue_the_status_bar_uses_the_admitted_glyph() {
        let mut state = empty_state();
        state.gates = vec![gate("g-1", "proceed?")];
        state.attention = vec![item("a-1", "one more")];

        let (dump, _, _) = dump_frame(72, 10, &state, None);
        assert_eq!(
            dump.lines().nth(9),
            Some("QUEUE  !2  as-of -"),
            "the status bar counts what is audible, in its own cells:\n{dump}"
        );
        assert!(
            !dump.contains('\u{26a0}'),
            "U+26A0 is banned from the cell buffer"
        );
    }

    #[test]
    fn queue_an_authority_row_counts_its_receipts() {
        let state = workday_state();
        let (dump, _, _) = dump_frame(72, 14, &state, None);
        assert!(
            dump.contains("3x since 09:14"),
            "the receipt trail is the occurrence count:\n{dump}"
        );

        // The same receipts against a non-authority row: no count. The rule
        // is display-on-authority-rows-only, not wherever a join happens to
        // match.
        let mut plain = item("a-plain", "not authority");
        plain.subject_ref = Some("command/cmd-7".into());
        assert_eq!(occurrences(&plain, &state.receipts), None);
    }

    #[test]
    fn queue_message_arrival_is_a_standing_row_without_motion() {
        let state = workday_state();
        let (dump, _, _) = dump_frame(72, 14, &state, None);
        assert!(
            dump.contains("msg 11:58  researcher to orchestrator  (findings)"),
            "arrival is a standing row, not a motion event:\n{dump}"
        );
    }

    #[test]
    fn queue_resolved_items_demote_to_recent_under_the_rearm_header() {
        let state = workday_state();
        let (dump, _, _) = dump_frame(72, 14, &state, None);
        let recent_at = dump
            .find("recent -- resolve is re-arm")
            .expect("the recent header says what demotion means");
        let resolved_at = dump
            .find("runner pool exhausted")
            .expect("the resolved item renders");
        assert!(
            resolved_at > recent_at,
            "a resolved item lives below the recent header, not among the live rows"
        );
    }

    #[test]
    fn queue_muted_and_acked_rows_stay_visible_in_words() {
        let state = workday_state();
        let (dump, _, _) = dump_frame(72, 14, &state, None);
        assert!(
            dump.contains("disk pressure on the build host  acked"),
            "an acked row is quiet, not gone, summary first:\n{dump}"
        );
        assert!(
            dump.contains("mcp facade flapping  muted until 18:00"),
            "a muted row says when it comes back, after the summary:\n{dump}"
        );

        // A mute whose deadline has passed is audible again — the client
        // unmutes by time alone, no verb required.
        let mut woke = item("a-woke", "was muted");
        woke.muted_until = Some(ts("2026-08-06T11:00:00Z"));
        assert!(audible(&woke, &ts(NOW)));
    }

    #[test]
    fn queue_selection_is_a_reverse_video_space_in_column_zero() {
        let mut state = empty_state();
        state.attention = vec![item("a-1", "the row")];
        let target = QueueTarget::Attention(AttentionItemId::new("a-1"));

        let (_, _, buf) = dump_frame(40, 6, &state, Some(&target));
        // The row renders after the verdict line, so it sits at y=1.
        let accent = &buf[(0, 1)];
        assert_eq!(accent.symbol(), " ", "the accent is a space, not a glyph");
        assert!(
            accent.style().add_modifier.contains(Modifier::REVERSED),
            "the accent is reverse video — the tier-independent primitive"
        );
        let unselected = &buf[(0, 0)];
        assert!(
            !unselected.style().add_modifier.contains(Modifier::REVERSED),
            "only the selected row carries the accent"
        );
    }

    #[test]
    fn queue_clicks_land_on_rows_and_a_miss_stays_a_miss() {
        let mut state = empty_state();
        state.attention = vec![item("a-1", "clickable")];

        let (_, hits, _) = dump_frame(40, 6, &state, None);
        assert_eq!(
            hits.hit(5, 1),
            Some(&QueueTarget::Attention(AttentionItemId::new("a-1"))),
            "the attention row is a click target"
        );
        assert_eq!(hits.hit(5, 0), None, "the verdict line is not actionable");
        assert_eq!(hits.hit(5, 2), None, "one row off is a miss");
    }

    #[test]
    fn queue_ack_and_mute_build_the_typed_commands_and_refuse_gates() {
        let attention = QueueTarget::Attention(AttentionItemId::new("a-1"));
        let gate_target = QueueTarget::Gate(GateId::new("g-1"));

        assert_eq!(
            ack(&attention),
            Some(KernelCommand::AckAttention {
                attention_item_id: AttentionItemId::new("a-1"),
            })
        );
        let until = ts("2026-08-06T13:00:00Z");
        assert_eq!(
            mute(&attention, until.clone()),
            Some(KernelCommand::MuteAttention {
                attention_item_id: AttentionItemId::new("a-1"),
                muted_until: until,
            })
        );
        // A gate is decided, never quieted.
        assert_eq!(ack(&gate_target), None);
        assert_eq!(mute(&gate_target, ts(NOW)), None);
    }

    #[test]
    fn queue_overflow_names_the_cut() {
        let mut state = empty_state();
        state.attention = (0..12)
            .map(|i| item(&format!("a-{i:02}"), &format!("problem {i}")))
            .collect();

        let (dump, _, _) = dump_frame(40, 6, &state, None);
        // 16 built rows, 4 painted: the notice states the exact cut.
        assert!(
            dump.contains("+12 more"),
            "an overflowing body names the cut instead of silently truncating:\n{dump}"
        );
    }

    #[test]
    fn queue_every_cell_glyph_is_ascii_or_an_admitted_mark() {
        // The admission rule (taste-gate item 5, a hard lock): EAW=Ambiguous
        // codepoints shear frames on ambiguous-wide terminals, so nothing
        // reaches the cell buffer unless it passes the shared admission
        // predicate. This walks the busiest frame so a stray em dash or
        // ellipsis in any row's text fails here, not on an operator's screen.
        let (dump, _, _) = dump_frame(72, 14, &workday_state(), None);
        for ch in dump.chars() {
            assert!(
                ch.is_ascii() || gwk_theme::marks::is_admissible(ch),
                "unadmitted glyph {ch:?} in the cell buffer"
            );
        }
    }

    #[test]
    fn queue_unsafe_wire_glyphs_are_escaped_before_paint() {
        let mut state = empty_state();
        state.attention = vec![item("a-unsafe", "unsafe ◆ 你好 ⚠")];

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
    fn queue_ack_quiets_the_count_with_the_row() {
        // Quieting is one act with two halves: the verdict line, the status
        // bar, and the `!` marks must always agree. An acked row that still
        // counted would be a number the frame cannot corroborate.
        let mut seen = item("a-seen", "already seen");
        seen.acked_at = Some(ts("2026-08-06T10:00:00Z"));
        let mut state = empty_state();
        state.attention = vec![seen, item("a-loud", "still loud")];

        let (dump, _, _) = dump_frame(60, 8, &state, None);
        assert!(
            dump.contains("1 need attention"),
            "the acked item leaves the count:\n{dump}"
        );
        assert!(
            dump.contains("!1"),
            "the status bar agrees with the verdict line:\n{dump}"
        );
        assert!(
            dump.contains("already seen  acked"),
            "the acked row is quiet, not gone:\n{dump}"
        );
    }

    #[test]
    fn queue_selection_layers_the_foreground_and_the_mark_keeps_its_colour() {
        // STM Fork B at a colour tier: the selected row's text takes the
        // `selection` foreground LAYERED over its style, and the mark cell
        // keeps the state binding's colour — the state colour is the mark's
        // meaning.
        let mut state = empty_state();
        state.attention = vec![item("a-1", "the row")];
        let target = QueueTarget::Attention(AttentionItemId::new("a-1"));

        let (_, _, buf) = dump_frame_tier(40, 6, &state, Some(&target), ColorTier::Truecolor);
        let warn_fg = theme::state_style(binding("needs_attention"), ColorTier::Truecolor).fg;
        let selection_fg = gwk_theme::SIGNAL
            .iter()
            .find(|t| t.name == "selection")
            .and_then(|t| theme::token_style(t, ColorTier::Truecolor).fg);
        assert!(warn_fg.is_some() && selection_fg.is_some(), "tier emits fg");
        // The row sits at y=1 under the verdict line: accent 0, mark 1, text 3.
        assert_eq!(buf[(1, 1)].style().fg, warn_fg, "the mark keeps its state");
        assert_eq!(
            buf[(3, 1)].style().fg,
            selection_fg,
            "the text takes the selection foreground"
        );
    }

    #[test]
    fn queue_a_narrow_area_stays_inside_its_rect() {
        // The widget paints only the cells it was given: in a split layout a
        // stray write is a scribble on the neighbouring pane.
        let mut state = empty_state();
        state.attention = vec![item("a-1", "a row wider than the pane")];
        let target = QueueTarget::Attention(AttentionItemId::new("a-1"));
        let mut hits = HitMap::new();

        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 6));
        render(
            Rect::new(0, 0, 2, 6),
            &mut buf,
            &state,
            Some(&target),
            ColorTier::Mono,
            GlyphSet::Unicode,
            &mut hits,
        );
        for y in 0..6 {
            for x in 2..40 {
                assert_eq!(
                    buf[(x, y)].symbol(),
                    " ",
                    "painted outside its area at ({x},{y})"
                );
            }
        }

        // Zero width: nothing paints at all.
        let mut empty_buf = Buffer::empty(Rect::new(0, 0, 40, 6));
        render(
            Rect::new(0, 0, 0, 6),
            &mut empty_buf,
            &state,
            Some(&target),
            ColorTier::Mono,
            GlyphSet::Unicode,
            &mut hits,
        );
        assert_eq!(empty_buf, Buffer::empty(Rect::new(0, 0, 40, 6)));
    }
}
