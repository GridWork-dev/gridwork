//! Round 2 — the FLEET lens (grill G1: `FLEET = agents · leases · cost`).
//!
//! MOCKUP PAINTER ONLY — no production lens code. It renders what the audit
//! flagged as never rendered:
//! **the DispatchNode <-> Attempt <-> EngineSession <-> CostEntry joins**,
//! and **a time axis on cost**.
//!
//! **RULED `work` (picker, 2026-08-11):** one row per attempt, every join
//! folded onto it — sessions, dispatch subtree size, lease + flags, rolled
//! spend and tokens — with the spend-per-hour chart beneath. One grain, one
//! comparable column set. The losing `resource` candidate (three stacked
//! resource sections, cross-referenced) is retired; its frames are in this
//! branch's history at `8d4cd30`. Its one real advantage — a resource with
//! no attempt, like the expired `ls-release` lease, gets no row here — is
//! carried as an open follow-up (an "unclaimed" footer).
//!
//! ## What the join is worth
//!
//! `CostEntry` carries three DB-enforced foreign keys (`attempt_id`,
//! `engine_session_id`, `dispatch_node_id`) with a CHECK requiring at least
//! one — built for a query nobody ever wrote. The shipped rollup groups
//! only by `(engine, model)`. Two of the twelve seeded entries are
//! attributable to an attempt ONLY through a join: `ce-08` reaches
//! `at-pty-impl` through `d-pty-recon`, and `ce-09` reaches `at-tui-impl`
//! through `es-tui-impl`. Without the join those $0.43 are invisible at the
//! unit of work; with it, `at-tui-impl` is correctly the second most
//! expensive attempt of the day.
//!
//! **Attribution precedence (the ruling this round records):** an entry is
//! counted exactly ONCE, resolved `attempt_id` -> `engine_session_id` ->
//! `dispatch_node_id`. Never double-counted across two groupings, which the
//! DB schema permits and the CHECK does not prevent. An entry resolving to
//! no attempt is counted in an explicit `unattributed` tally rather than
//! dropped.
//!
//! ## Honesty rules exercised here
//!
//! - Unpriced entries (`cost_micros` absent) are never treated as zero: a
//!   spend cell reads `$0.09 +1u`, and the footer states priced/unpriced
//!   counts. The seeded day has 10 priced and 2 unpriced entries.
//! - The chart is drawn from `#` columns. The ratified mark inventory has
//!   NO bar or sparkline glyph, and Unicode block elements are
//!   East-Asian-Width Ambiguous — inadmissible under the theme's own rule.
//!   A real time axis therefore needs either ASCII bars (this) or a new
//!   admissible mark; recorded as an open ask in DESIGN-NOTES.
//! - `AttemptState` has ten variants against `AgentState`'s eleven; the
//!   mapping is explicit in [`attempt_glyph_state`], with `Leased` (no
//!   Hall equivalent) reading as `queued` and `Succeeded` as `done`.

mod common;

#[path = "mockups/shared.rs"]
mod shared;

use common::{assert_matches_golden, dump_frame};
use gwk_domain::entity::{Attempt, CostEntry, DispatchNode, EngineSession, Lease};
use gwk_domain::fsm::{AttemptState, LeaseState};
use gwk_domain::ids::AttemptId;
use gwk_theme::marks::GlyphSet;
use gwk_theme::tier::ColorTier;
use gwk_tui::board::{BoardState, BoardView};
use gwk_tui::hall::AgentState;
use gwk_tui::theme::state_style;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use shared::{
    binding, bold, dollars, put, put_keybar, put_right, put_rule, short_role, state_glyph, style,
    tier_badge, tokens,
};

/// The seeded clock, in minutes past midnight.
const NOW_MINUTES: u32 = 17 * 60 + 30;

// ---------------------------------------------------------------------------
// Derivations over the seeded estate
// ---------------------------------------------------------------------------

/// Minutes past midnight from a seeded `2026-08-11T HH:MM:SS Z` stamp.
/// Every seeded fact is same-day, so a same-day reading is honest here; a
/// real lens takes a duration from the kernel rather than parsing text.
fn minutes(stamp: &str) -> u32 {
    let hour: u32 = stamp[11..13].parse().expect("hour");
    let minute: u32 = stamp[14..16].parse().expect("minute");
    hour * 60 + minute
}

fn age(stamp: &str) -> String {
    let elapsed = NOW_MINUTES.saturating_sub(minutes(stamp));
    if elapsed < 60 {
        format!("{elapsed}m")
    } else {
        format!("{}h{:02}", elapsed / 60, elapsed % 60)
    }
}

/// `AttemptState` (ten variants) onto the eleven-variant `AgentState` the
/// ratified mark table is keyed by. `Leased` has no Hall equivalent and
/// reads as waiting; `Succeeded` is Hall's `Done`.
fn attempt_glyph_state(state: AttemptState) -> AgentState {
    match state {
        AttemptState::Queued | AttemptState::Leased => AgentState::Queued,
        AttemptState::Starting => AgentState::Starting,
        AttemptState::Running => AgentState::Running,
        AttemptState::Blocked => AgentState::Blocked,
        AttemptState::Canceling => AgentState::Canceling,
        AttemptState::Canceled => AgentState::Canceled,
        AttemptState::Failed => AgentState::Failed,
        AttemptState::Unknown => AgentState::Unknown,
        AttemptState::Succeeded => AgentState::Done,
    }
}

fn attempt_state_word(state: AttemptState) -> &'static str {
    match state {
        AttemptState::Queued => "queued",
        AttemptState::Leased => "leased",
        AttemptState::Starting => "starting",
        AttemptState::Running => "running",
        AttemptState::Blocked => "blocked",
        AttemptState::Canceling => "canceling",
        AttemptState::Canceled => "canceled",
        AttemptState::Failed => "failed",
        AttemptState::Unknown => "unknown",
        AttemptState::Succeeded => "done",
    }
}

/// One attempt's rolled-up spend. `unpriced` is carried beside the total
/// rather than folded into it — a missing `cost_micros` is an unknown, not
/// a zero.
#[derive(Default, Clone, Copy)]
struct Spend {
    micros: u64,
    priced: usize,
    unpriced: usize,
    input: u64,
    output: u64,
}

impl Spend {
    fn add(&mut self, entry: &CostEntry) {
        match entry.cost_micros {
            Some(micros) => {
                self.micros += micros.value();
                self.priced += 1;
            }
            None => self.unpriced += 1,
        }
        self.input += entry.input_tokens.map_or(0, |count| count.value());
        self.output += entry.output_tokens.map_or(0, |count| count.value());
    }

    fn entries(&self) -> usize {
        self.priced + self.unpriced
    }

    /// Three distinct readings, never collapsed: no cost entry at all is
    /// `-` (nothing recorded), entries that are all unpriced are `+Nu`
    /// (spend unknown, not zero), and a priced total carries its unpriced
    /// remainder as a floor marker.
    fn text(&self) -> String {
        match (self.priced, self.unpriced) {
            (0, 0) => "-".to_owned(),
            (0, unpriced) => format!("+{unpriced}u"),
            (_, 0) => dollars(self.micros),
            (_, unpriced) => format!("{} +{unpriced}u", dollars(self.micros)),
        }
    }

    fn token_text(&self) -> String {
        if self.entries() == 0 {
            return "-".to_owned();
        }
        format!("{}/{}", tokens(self.input), tokens(self.output))
    }
}

/// Resolve a cost entry to the attempt that owes it, counting it exactly
/// once: direct `attempt_id`, else through its engine session, else through
/// its dispatch node. `None` = attributable to no attempt (tallied, never
/// dropped).
fn cost_attempt(
    entry: &CostEntry,
    sessions: &[EngineSession],
    nodes: &[DispatchNode],
) -> Option<AttemptId> {
    if let Some(attempt) = &entry.attempt_id {
        return Some(attempt.clone());
    }
    if let Some(session_id) = &entry.engine_session_id
        && let Some(session) = sessions.iter().find(|session| &session.id == session_id)
    {
        return Some(session.attempt_id.clone());
    }
    if let Some(node_id) = &entry.dispatch_node_id
        && let Some(node) = nodes.iter().find(|node| &node.id == node_id)
    {
        return node.attempt_id.clone();
    }
    None
}

fn spend_for(state: &BoardState, attempt: &AttemptId) -> Spend {
    let mut spend = Spend::default();
    for entry in &state.costs {
        if cost_attempt(entry, &state.sessions, &state.nodes).as_ref() == Some(attempt) {
            spend.add(entry);
        }
    }
    spend
}

fn unattributed(state: &BoardState) -> usize {
    state
        .costs
        .iter()
        .filter(|entry| cost_attempt(entry, &state.sessions, &state.nodes).is_none())
        .count()
}

fn lease_for<'a>(state: &'a BoardState, attempt: &Attempt) -> Option<&'a Lease> {
    let id = attempt.worktree_lease_id.as_ref()?;
    state.leases.iter().find(|lease| &lease.id == id)
}

/// `d` = dirty, `u` = unpushed — the two facts that decide whether a
/// worktree can be reclaimed. Both are booleans the shipped fleet summary
/// carries and no view renders.
fn lease_flags(lease: &Lease) -> String {
    let mut flags = String::new();
    if lease.dirty {
        flags.push('d');
    }
    if lease.unpushed {
        flags.push('u');
    }
    flags
}

fn subtree_size(state: &BoardState, attempt: &AttemptId) -> usize {
    state
        .nodes
        .iter()
        .filter(|node| node.attempt_id.as_ref() == Some(attempt))
        .count()
}

fn live_sessions(state: &BoardState, attempt: &AttemptId) -> usize {
    state
        .sessions
        .iter()
        .filter(|session| &session.attempt_id == attempt && session.ended_at.is_none())
        .count()
}

fn task_label(state: &BoardState, attempt: &Attempt) -> String {
    state
        .tasks
        .iter()
        .find(|task| task.id == attempt.task_id)
        .map(|task| {
            task.id
                .as_str()
                .strip_prefix("t-")
                .unwrap_or_else(|| task.id.as_str())
                .to_owned()
        })
        .unwrap_or_else(|| "-".to_owned())
}

fn role_label(attempt: &Attempt) -> &str {
    attempt.role.as_deref().map_or("-", short_role)
}

// ---------------------------------------------------------------------------
// Shared chrome
// ---------------------------------------------------------------------------

fn totals(state: &BoardState) -> (u64, usize, usize) {
    let mut micros = 0;
    let mut priced = 0;
    let mut unpriced = 0;
    for entry in &state.costs {
        match entry.cost_micros {
            Some(value) => {
                micros += value.value();
                priced += 1;
            }
            None => unpriced += 1,
        }
    }
    (micros, priced, unpriced)
}

fn paint_header(
    buf: &mut Buffer,
    area: Rect,
    tier: ColorTier,
    glyphs: GlyphSet,
    state: &BoardState,
) {
    let compact = area.width < 100;
    let (micros, _, _) = totals(state);
    let live = state
        .attempts
        .iter()
        .filter(|attempt| {
            matches!(
                attempt.state,
                AttemptState::Running | AttemptState::Starting | AttemptState::Canceling
            )
        })
        .count();
    let held = state
        .leases
        .iter()
        .filter(|lease| lease.state == LeaseState::Held)
        .count();

    put(buf, area, 1, 0, "FLEET", bold("fg", tier));
    let summary = if compact {
        format!(
            "{} attempts  {live} live  {} today",
            state.attempts.len(),
            dollars(micros),
        )
    } else {
        format!(
            "{} attempts  {live} live  {} sessions  {} leases ({held} held)  {} today",
            state.attempts.len(),
            state.sessions.len(),
            state.leases.len(),
            dollars(micros),
        )
    };
    put(buf, area, 9, 0, &summary, style("fg", tier));

    let badge = tier_badge(tier, glyphs);
    let right = if compact {
        format!("{badge}  17:30")
    } else {
        format!("tier {badge}  as-of 221  17:30")
    };
    put_right(buf, area, 0, &right, style("muted", tier));
}

/// The spend-per-hour chart: three `#` rows over the day's buckets. Bars
/// are ASCII because the ratified inventory has no bar mark and Unicode
/// block elements are Ambiguous-width.
fn paint_spend_chart(buf: &mut Buffer, area: Rect, top: u16, tier: ColorTier, state: &BoardState) {
    const ROWS: u32 = 3;
    let first = 9u32;
    let last = 17u32;
    let step: u16 = if area.width < 100 { 6 } else { 8 };
    let left: u16 = 8;

    let mut buckets = vec![0u64; (last - first + 1) as usize];
    for entry in &state.costs {
        let Some(micros) = entry.cost_micros else {
            continue;
        };
        let hour = minutes(entry.recorded_at.as_str()) / 60;
        if hour >= first && hour <= last {
            buckets[(hour - first) as usize] += micros.value();
        }
    }
    let peak = buckets.iter().copied().max().unwrap_or(0).max(1);

    // Any hour that spent anything gets at least one row. A proportional
    // scale alone hides four of this day's five spending hours behind the
    // 15:00 spike — a chart that renders real spend as blank is worse than
    // no chart.
    let heights: Vec<u32> = buckets
        .iter()
        .map(|value| {
            if *value == 0 {
                0
            } else {
                ((value * u64::from(ROWS)).div_ceil(peak) as u32).clamp(1, ROWS)
            }
        })
        .collect();

    let (_, priced, unpriced) = totals(state);
    let caption = format!(
        "SPEND / HOUR   {} priced   {unpriced} unpriced   {} unattributed",
        priced,
        unattributed(state),
    );
    put(buf, area, 2, top, &caption, bold("fg", tier));

    for row in 0..ROWS {
        let y = top + 1 + row as u16;
        let level = ROWS - row;
        let label = match row {
            0 => dollars(peak),
            row if row == ROWS - 1 => "> $0".to_owned(),
            _ => String::new(),
        };
        put(buf, area, 2, y, &label, style("muted", tier));
        for (index, height) in heights.iter().enumerate() {
            if *height >= level {
                let x = left + index as u16 * step;
                put(buf, area, x, y, "####", style("hue", tier));
            }
        }
    }

    let axis_y = top + 1 + ROWS as u16;
    for hour in first..=last {
        let x = left + (hour - first) as u16 * step;
        put(
            buf,
            area,
            x,
            axis_y,
            &format!("{hour:02}h"),
            style("muted", tier),
        );
    }
}

fn paint_keybar(buf: &mut Buffer, area: Rect, tier: ColorTier) {
    put_keybar(
        buf,
        area,
        tier,
        " : go   / filter   enter open   s stop   b budget   c cost   q quit",
        " : go   / filter   enter open   s stop   q quit",
    );
}

// ---------------------------------------------------------------------------
// Candidate A — work grain: one row per attempt, every join folded on
// ---------------------------------------------------------------------------

fn paint_work(area: Rect, buf: &mut Buffer, tier: ColorTier, glyphs: GlyphSet) {
    let state = common::estate::estate_board_state(BoardView::Fleet);
    let wide = area.width >= 100;
    paint_header(buf, area, tier, glyphs, &state);

    // Column origins. `SUB` is the dispatch subtree size and `SES` the live
    // engine-session count — the two joins that tell an operator whether a
    // row is one process or a fan-out.
    let columns: &[(u16, &str)] = if wide {
        &[
            (2, "ATTEMPT"),
            (21, "TASK"),
            (36, "ENGINE"),
            (44, "ROLE"),
            (54, "STATE"),
            (67, "SUB"),
            (72, "SES"),
            (77, "LEASE"),
            (91, "TOKENS"),
            (103, "SPEND"),
            (115, "AGE"),
        ]
    } else {
        &[
            (2, "ATTEMPT"),
            (21, "ENGINE"),
            (29, "ROLE"),
            (39, "STATE"),
            (52, "SPEND"),
            (65, "AGE"),
        ]
    };
    for (x, label) in columns {
        put(buf, area, *x, 2, label, style("muted", tier));
    }

    let mut y = 3;
    for (index, attempt) in state.attempts.iter().enumerate() {
        if y >= area.height - 8 {
            let remaining = state.attempts.len() - index;
            put(
                buf,
                area,
                2,
                y,
                &format!("+{remaining} more"),
                style("muted", tier),
            );
            y += 1;
            break;
        }

        let paint = state_style(binding(attempt_glyph_state(attempt.state)), tier);
        let focused = index == 1;
        let mark = state_glyph(attempt_glyph_state(attempt.state), glyphs);
        let spend = spend_for(&state, &attempt.id);
        let lease = lease_for(&state, attempt);
        let lease_text = lease.map_or_else(
            || "-".to_owned(),
            |lease| {
                let flags = lease_flags(lease);
                if flags.is_empty() {
                    lease.id.as_str().to_owned()
                } else {
                    format!("{} {flags}", lease.id.as_str())
                }
            },
        );
        let sessions = live_sessions(&state, &attempt.id);
        let subtree = subtree_size(&state, &attempt.id);

        put(
            buf,
            area,
            0,
            y,
            if focused { ">" } else { " " },
            bold("focus", tier),
        );
        put(buf, area, 2, y, attempt.id.as_str(), paint);
        if wide {
            put(
                buf,
                area,
                21,
                y,
                &task_label(&state, attempt),
                style("muted", tier),
            );
        }
        let engine_x = if wide { 36 } else { 21 };
        let role_x = if wide { 44 } else { 29 };
        let state_x = if wide { 54 } else { 39 };
        put(
            buf,
            area,
            engine_x,
            y,
            attempt.engine.as_str(),
            style("muted", tier),
        );
        put(buf, area, role_x, y, role_label(attempt), style("fg", tier));
        put(
            buf,
            area,
            state_x,
            y,
            &format!("{mark} {}", attempt_state_word(attempt.state)),
            paint,
        );
        let count = |value: usize| {
            if value == 0 {
                "-".to_owned()
            } else {
                value.to_string()
            }
        };
        let (spend_x, age_x) = if wide {
            put(buf, area, 67, y, &count(subtree), style("muted", tier));
            put(buf, area, 72, y, &count(sessions), style("muted", tier));
            put(buf, area, 77, y, &lease_text, style("muted", tier));
            put(buf, area, 91, y, &spend.token_text(), style("muted", tier));
            (103, 115)
        } else {
            (52, 65)
        };
        put(buf, area, spend_x, y, &spend.text(), style("fg", tier));
        put(
            buf,
            area,
            age_x,
            y,
            &age(attempt.created_at.as_str()),
            style("muted", tier),
        );
        y += 1;
    }

    put_rule(buf, area, y, tier);
    paint_spend_chart(buf, area, y + 1, tier, &state);
    paint_keybar(buf, area, tier);
}

// ---------------------------------------------------------------------------
// The golden matrix
// ---------------------------------------------------------------------------

fn check(candidate: &str, width: u16, height: u16, tier: ColorTier, glyphs: GlyphSet) {
    let glyph_name = match glyphs {
        GlyphSet::Unicode => "unicode",
        GlyphSet::Ascii => "ascii",
    };
    let name = format!(
        "mock-fleet-{candidate}-{width}x{height}-{}-{glyph_name}",
        tier.as_str()
    );
    assert_matches_golden(&name, &dump_frame(width, height, tier, glyphs, paint_work));
}

#[test]
fn work_at_120x40() {
    check("work", 120, 40, ColorTier::Truecolor, GlyphSet::Unicode);
}

#[test]
fn work_at_80x24() {
    check("work", 80, 24, ColorTier::Truecolor, GlyphSet::Unicode);
}

#[test]
fn work_degraded_mono_ascii() {
    check("work", 120, 40, ColorTier::Mono, GlyphSet::Ascii);
}
