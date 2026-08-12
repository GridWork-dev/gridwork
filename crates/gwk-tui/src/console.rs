//! The five-lens console surface outside the clean-room multiplexer.
//!
//! This module owns lens chrome and the shared row/keybar grammar. It paints
//! projection values supplied by the binary; it performs no I/O and submits no
//! commands.

use gwk_domain::entity::{Attempt, CostEntry, DispatchNode, EngineSession, Lease};
use gwk_domain::fsm::{AttemptState, LeaseState, StateMachine as _};
use gwk_domain::ids::{AttemptId, Timestamp};
use gwk_theme::marks::{GlyphSet, StateBinding};
use gwk_theme::tier::ColorTier;
use gwk_theme::{SIGNAL, Token};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use crate::board::{BoardState, BoardTarget};
use crate::hall::{Agent, AgentState, District, FrameInput, HallTarget};
use crate::input::HitMap;
use crate::theme;

pub const MIN_WIDTH: u16 = 80;
pub const MIN_HEIGHT: u16 = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadState {
    Loading,
    Ready,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HallContext {
    pub now: Timestamp,
    pub running: usize,
    pub attention: usize,
    pub cost_micros: u64,
    pub load: LoadState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetContext {
    pub now: Timestamp,
    pub load: LoadState,
}

pub fn render_hall(
    area: Rect,
    buf: &mut Buffer,
    input: &FrameInput,
    context: &HallContext,
    tier: ColorTier,
    glyphs: GlyphSet,
    hits: &mut HitMap<HallTarget>,
) {
    render_hall_at(area, buf, input, context, tier, glyphs, 0, hits);
}

#[allow(clippy::too_many_arguments)]
pub fn render_hall_at(
    area: Rect,
    buf: &mut Buffer,
    input: &FrameInput,
    context: &HallContext,
    tier: ColorTier,
    glyphs: GlyphSet,
    phase: usize,
    hits: &mut HitMap<HallTarget>,
) {
    hits.clear();
    if area.width == 0 || area.height == 0 {
        return;
    }
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        render_too_small(area, buf, tier);
        return;
    }

    let compact = area.width < 100;
    paint_hall_header(area, buf, input, context, tier, glyphs, compact);
    match context.load {
        LoadState::Loading => {
            put(
                buf,
                area,
                2,
                3,
                "loading estate projections...",
                style("muted", tier),
            );
        }
        LoadState::Ready if input.districts.is_empty() => {
            put(
                buf,
                area,
                2,
                3,
                "estate idle -- no active districts",
                style("muted", tier),
            );
        }
        LoadState::Ready => {
            paint_hall_body(area, buf, input, context, tier, glyphs, phase, hits);
        }
    }

    put_keybar(
        buf,
        area,
        tier,
        " : go   / filter   enter open   j/k district   [ ] page   m motion   q quit",
        " : go   / filter   enter open   j/k   q quit",
    );
    if area.width >= 100 && context.load == LoadState::Ready {
        let agents = input
            .districts
            .iter()
            .flat_map(|district| &district.stations)
            .map(|station| station.agents.len())
            .sum::<usize>();
        put_right(
            buf,
            area,
            area.height - 1,
            &format!("{} districts  {agents} agents", input.districts.len()),
            style("muted", tier),
        );
    }
}

fn paint_hall_header(
    area: Rect,
    buf: &mut Buffer,
    input: &FrameInput,
    context: &HallContext,
    tier: ColorTier,
    glyphs: GlyphSet,
    compact: bool,
) {
    put(buf, area, 1, 0, "GRIDWORK", bold("fg", tier));
    put(
        buf,
        area,
        11,
        0,
        &format!("run {}", context.running),
        style("hue", tier),
    );
    put(
        buf,
        area,
        18,
        0,
        &format!("!{}", context.attention),
        style("warn", tier),
    );
    put(
        buf,
        area,
        22,
        0,
        &format!("{} today", dollars(context.cost_micros)),
        style("fg", tier),
    );

    let badge = tier_badge(tier, glyphs);
    let clock = hhmm(&context.now);
    let right = if compact {
        format!("{badge}  {clock}")
    } else {
        let watermark = input
            .watermark
            .map_or_else(|| "-".to_owned(), |value| value.to_string());
        format!("tier {badge}  as-of {watermark}  {clock}")
    };
    put_right(buf, area, 0, &right, style("muted", tier));
}

#[allow(clippy::too_many_arguments)]
fn paint_hall_body(
    area: Rect,
    buf: &mut Buffer,
    input: &FrameInput,
    context: &HallContext,
    tier: ColorTier,
    glyphs: GlyphSet,
    phase: usize,
    hits: &mut HitMap<HallTarget>,
) {
    let compact = area.width < 100;
    let mut y = 2;
    for district in &input.districts {
        if y + 2 >= area.height {
            break;
        }
        paint_district_heading(area, buf, input, district, y, tier);
        y += 1;

        let mut x = 2u16;
        for station in &district.stations {
            let station_label =
                theme::safe_text(&station.label, area.width.saturating_sub(x) as usize);
            put(
                buf,
                area,
                x,
                y,
                station_label.as_ref(),
                style("muted", tier),
            );
            x = x.saturating_add(station_label.chars().count() as u16 + 2);
            for agent in &station.agents {
                let mark = state_glyph(agent.state, glyphs, phase);
                let label = identity(agent);
                let cell = match agent
                    .started_at
                    .as_ref()
                    .and_then(|started| elapsed(&context.now, started))
                {
                    Some(elapsed) if !compact => format!("{mark} {label} {elapsed}"),
                    _ => format!("{mark} {label}"),
                };
                let safe_cell = theme::safe_text(&cell, area.width.saturating_sub(x) as usize);
                put(
                    buf,
                    area,
                    x,
                    y,
                    safe_cell.as_ref(),
                    theme::state_style(binding(agent.state), tier),
                );
                hits.register(
                    Rect::new(area.x + x, area.y + y, safe_cell.chars().count() as u16, 1),
                    HallTarget::Agent(agent.id.clone()),
                );
                x = x.saturating_add(safe_cell.chars().count() as u16 + 3);
            }
            x = x.saturating_add(2);
        }
        y += 2;
    }
}

fn paint_district_heading(
    area: Rect,
    buf: &mut Buffer,
    input: &FrameInput,
    district: &District,
    y: u16,
    tier: ColorTier,
) {
    let focused = input
        .focus
        .as_ref()
        .is_some_and(|focus| focus.district == district.id);
    let text = format!(
        "{} {}",
        if focused { ">" } else { " " },
        district.label.to_uppercase()
    );
    let safe_text = theme::safe_text(&text, area.width as usize);
    put(
        buf,
        area,
        0,
        y,
        safe_text.as_ref(),
        if focused {
            bold("focus", tier)
        } else {
            bold("fg", tier)
        },
    );
    let mut x = safe_text.chars().count() as u16 + 2;
    let attention = input
        .attention
        .iter()
        .filter(|item| item.unresolved && item.district == district.id)
        .count();
    if attention > 0 {
        let badge = format!("!{attention}");
        put(buf, area, x, y, &badge, style("warn", tier));
        x = x.saturating_add(badge.chars().count() as u16 + 2);
    }
    if district.aged_done > 0 {
        put(
            buf,
            area,
            x,
            y,
            &format!("+{} done", district.aged_done),
            style("muted", tier),
        );
    }
}

#[allow(clippy::too_many_arguments)]
pub fn render_fleet(
    area: Rect,
    buf: &mut Buffer,
    state: &BoardState,
    context: &FleetContext,
    selected: Option<&BoardTarget>,
    tier: ColorTier,
    glyphs: GlyphSet,
    hits: &mut HitMap<BoardTarget>,
) {
    hits.clear();
    if area.width == 0 || area.height == 0 {
        return;
    }
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        render_too_small(area, buf, tier);
        return;
    }

    paint_fleet_header(area, buf, state, context, tier, glyphs);
    if context.load == LoadState::Loading {
        put(
            buf,
            area,
            2,
            3,
            "loading fleet projections...",
            style("muted", tier),
        );
        fleet_keybar(area, buf, tier);
        return;
    }

    let wide = area.width >= 120;
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

    let attempt_rows = area.height.saturating_sub(13) as usize;
    let selected_index = selected.and_then(|target| {
        let BoardTarget::Attempt(id) = target else {
            return None;
        };
        state.attempts.iter().position(|attempt| &attempt.id == id)
    });
    let needs_notice = state.attempts.len() > attempt_rows && attempt_rows > 1;
    let visible = attempt_rows
        .saturating_sub(usize::from(needs_notice))
        .min(state.attempts.len());
    let start = if visible == 0 {
        0
    } else {
        selected_index
            .filter(|index| *index >= visible)
            .map_or(0, |index| index + 1 - visible)
            .min(state.attempts.len().saturating_sub(visible))
    };
    let end = (start + visible).min(state.attempts.len());
    let mut y = 3;
    for attempt in &state.attempts[start..end] {
        paint_attempt_row(
            area, buf, y, state, context, attempt, selected, tier, glyphs, wide, hits,
        );
        y = y.saturating_add(1);
    }
    if needs_notice {
        let notice = if start == 0 {
            format!("+{} more", state.attempts.len() - end)
        } else if end == state.attempts.len() {
            format!("+{start} before")
        } else {
            format!("+{start} before  +{} more", state.attempts.len() - end)
        };
        put(buf, area, 2, y, &notice, style("muted", tier));
        y = y.saturating_add(1);
    }

    y = paint_unclaimed(area, buf, y, state, tier);
    put_rule(buf, area, y, tier);
    paint_spend_chart(buf, area, y.saturating_add(1), tier, state, &context.now);
    fleet_keybar(area, buf, tier);
}

fn paint_fleet_header(
    area: Rect,
    buf: &mut Buffer,
    state: &BoardState,
    context: &FleetContext,
    tier: ColorTier,
    glyphs: GlyphSet,
) {
    let compact = area.width < 100;
    let (micros, _, _) = cost_totals(state, &context.now);
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
            dollars(micros)
        )
    } else {
        format!(
            "{} attempts  {live} live  {} sessions  {} leases ({held} held)  {} today",
            state.attempts.len(),
            state.sessions.len(),
            state.leases.len(),
            dollars(micros)
        )
    };
    put(buf, area, 9, 0, &summary, style("fg", tier));
    let badge = tier_badge(tier, glyphs);
    let clock = hhmm(&context.now);
    let right = if compact {
        format!("{badge}  {clock}")
    } else {
        let watermark = state
            .watermark
            .map_or_else(|| "-".to_owned(), |value| value.to_string());
        format!("tier {badge}  as-of {watermark}  {clock}")
    };
    put_right(buf, area, 0, &right, style("muted", tier));
}

#[allow(clippy::too_many_arguments)]
fn paint_attempt_row(
    area: Rect,
    buf: &mut Buffer,
    y: u16,
    state: &BoardState,
    context: &FleetContext,
    attempt: &Attempt,
    selected: Option<&BoardTarget>,
    tier: ColorTier,
    glyphs: GlyphSet,
    wide: bool,
    hits: &mut HitMap<BoardTarget>,
) {
    let target = BoardTarget::Attempt(attempt.id.clone());
    let focused = selected == Some(&target);
    let face = attempt_agent_state(attempt.state);
    let paint = theme::state_style(binding(face), tier);
    let spend = spend_for(state, &attempt.id);
    let lease = lease_for(state, attempt);
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
    let count = |value: usize| {
        if value == 0 {
            "-".to_owned()
        } else {
            value.to_string()
        }
    };

    put(
        buf,
        area,
        0,
        y,
        if focused { ">" } else { " " },
        bold("focus", tier),
    );
    put_cell(buf, area, 2, 21, y, attempt.id.as_str(), paint);
    if wide {
        put_cell(
            buf,
            area,
            21,
            36,
            y,
            &task_label(state, attempt),
            style("muted", tier),
        );
    }
    let engine_x = if wide { 36 } else { 21 };
    let role_x = if wide { 44 } else { 29 };
    let state_x = if wide { 54 } else { 39 };
    put_cell(
        buf,
        area,
        engine_x,
        role_x,
        y,
        attempt.engine.as_str(),
        style("muted", tier),
    );
    put_cell(
        buf,
        area,
        role_x,
        state_x,
        y,
        attempt.role.as_deref().map_or("-", short_role),
        style("fg", tier),
    );
    put_cell(
        buf,
        area,
        state_x,
        if wide { 67 } else { 52 },
        y,
        &format!(
            "{} {}",
            state_glyph(face, glyphs, 0),
            attempt_state_word(attempt.state)
        ),
        paint,
    );
    let (spend_x, age_x) = if wide {
        put_cell(
            buf,
            area,
            67,
            72,
            y,
            &count(subtree_size(state, &attempt.id)),
            style("muted", tier),
        );
        put_cell(
            buf,
            area,
            72,
            77,
            y,
            &count(live_sessions(state, &attempt.id)),
            style("muted", tier),
        );
        put_cell(buf, area, 77, 91, y, &lease_text, style("muted", tier));
        put_value_cell(
            buf,
            area,
            91,
            103,
            y,
            &spend.token_text(),
            style("muted", tier),
        );
        (103, 115)
    } else {
        (52, 65)
    };
    put_value_cell(
        buf,
        area,
        spend_x,
        age_x,
        y,
        &spend.text(),
        style("fg", tier),
    );
    put_tail_cell(
        buf,
        area,
        age_x,
        y,
        &elapsed(&context.now, &attempt.created_at).unwrap_or_else(|| "-".to_owned()),
        style("muted", tier),
    );
    hits.register(Rect::new(area.x, area.y + y, area.width, 1), target);
}

fn paint_unclaimed(
    area: Rect,
    buf: &mut Buffer,
    mut y: u16,
    state: &BoardState,
    tier: ColorTier,
) -> u16 {
    let claimed: std::collections::BTreeSet<&str> = state
        .attempts
        .iter()
        .filter(|attempt| !attempt.state.is_terminal())
        .filter_map(|attempt| attempt.worktree_lease_id.as_ref().map(|id| id.as_str()))
        .collect();
    let leases: Vec<&Lease> = state
        .leases
        .iter()
        .filter(|lease| !claimed.contains(lease.id.as_str()))
        .collect();
    let worktrees: Vec<_> = state
        .worktrees
        .iter()
        .filter(|worktree| {
            worktree
                .lease_id
                .as_ref()
                .is_none_or(|lease| !claimed.contains(lease.as_str()))
        })
        .collect();
    let unattributed = unattributed(state);
    put(
        buf,
        area,
        2,
        y,
        &format!(
            "UNCLAIMED  {} lease{}  {} worktree{}  {unattributed} cost entr{}",
            leases.len(),
            plural_suffix(leases.len()),
            worktrees.len(),
            plural_suffix(worktrees.len()),
            if unattributed == 1 { "y" } else { "ies" },
        ),
        bold("fg", tier),
    );
    y = y.saturating_add(1);
    let mut facts = leases
        .iter()
        .map(|lease| format!("{} {}", lease.id.as_str(), lease_state_word(lease.state)))
        .chain(worktrees.iter().map(|worktree| {
            format!(
                "{} {}",
                worktree.id.as_str(),
                worktree.disposition.as_deref().unwrap_or("held")
            )
        }))
        .collect::<Vec<_>>();
    if unattributed > 0 {
        facts.push(format!("{unattributed} unattributed cost"));
    }
    put(buf, area, 4, y, &facts.join("   "), style("muted", tier));
    y.saturating_add(1)
}

fn plural_suffix(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

fn lease_state_word(state: LeaseState) -> &'static str {
    match state {
        LeaseState::Held => "held",
        LeaseState::Released => "released",
        LeaseState::Expired => "expired",
    }
}

fn fleet_keybar(area: Rect, buf: &mut Buffer, tier: ColorTier) {
    put_keybar(
        buf,
        area,
        tier,
        " : go   / filter   enter open   s stop   b budget   c cost   q quit",
        " : go   / filter   enter open   s stop   q quit",
    );
}

fn paint_spend_chart(
    buf: &mut Buffer,
    area: Rect,
    top: u16,
    tier: ColorTier,
    state: &BoardState,
    now: &Timestamp,
) {
    const ROWS: u32 = 3;
    let step: u16 = if area.width < 100 { 6 } else { 8 };
    let left = 8u16;
    // The axis follows the data — any same-day hour with spend gets a
    // bucket (the 09h-17h window in the mockups was the seeded workday, not
    // a ruling; the doctrine binds "any bucket with spend gets a mark").
    // When spend spans more hours than the row fits, the window keeps the
    // latest hours and the header says how many earlier ones it dropped.
    let mut day = [0u64; 24];
    let mut any = false;
    for entry in state
        .costs
        .iter()
        .filter(|entry| same_utc_date(&entry.recorded_at, now))
    {
        let Some(micros) = entry.cost_micros else {
            continue;
        };
        let Some(hour) = timestamp_hour(entry.recorded_at.as_str()) else {
            continue;
        };
        if let Some(bucket) = day.get_mut(hour as usize) {
            *bucket = bucket.saturating_add(micros.value());
            any = true;
        }
    }
    let (first, last) = if any {
        let earliest = day.iter().position(|value| *value > 0).unwrap_or(0) as u32;
        let latest = day.iter().rposition(|value| *value > 0).unwrap_or(0) as u32;
        (earliest, latest)
    } else {
        let hour = timestamp_hour(now.as_str()).unwrap_or(0);
        (hour, hour)
    };
    let max_buckets = u32::from((area.width.saturating_sub(left) / step).max(1));
    let span = (last - first + 1).min(max_buckets);
    let shown_first = last + 1 - span;
    let omitted_hours = day[..shown_first as usize]
        .iter()
        .filter(|value| **value > 0)
        .count();
    let buckets: Vec<u64> = (shown_first..=last)
        .map(|hour| day[hour as usize])
        .collect();
    let peak = buckets.iter().copied().max().unwrap_or(0).max(1);
    let heights = buckets
        .iter()
        .map(|value| {
            if *value == 0 {
                0
            } else {
                u32::try_from((u128::from(*value) * u128::from(ROWS)).div_ceil(u128::from(peak)))
                    .unwrap_or(ROWS)
                    .clamp(1, ROWS)
            }
        })
        .collect::<Vec<_>>();
    let (_, priced, unpriced) = cost_totals(state, now);
    let earlier = if omitted_hours > 0 {
        format!("   +{omitted_hours}h earlier")
    } else {
        String::new()
    };
    put(
        buf,
        area,
        2,
        top,
        &format!(
            "SPEND / HOUR   {priced} priced   {unpriced} unpriced   {} unattributed{earlier}",
            unattributed(state)
        ),
        bold("fg", tier),
    );
    for row in 0..ROWS {
        let y = top.saturating_add(1 + row as u16);
        let level = ROWS - row;
        let label = match row {
            0 => dollars(peak),
            value if value == ROWS - 1 => "> $0".to_owned(),
            _ => String::new(),
        };
        put(buf, area, 2, y, &label, style("muted", tier));
        for (index, height) in heights.iter().enumerate() {
            if *height >= level {
                put(
                    buf,
                    area,
                    left.saturating_add(index as u16 * step),
                    y,
                    "####",
                    style("hue", tier),
                );
            }
        }
    }
    let axis_y = top.saturating_add(1 + ROWS as u16);
    for hour in shown_first..=last {
        put(
            buf,
            area,
            left.saturating_add((hour - shown_first) as u16 * step),
            axis_y,
            &format!("{hour:02}h"),
            style("muted", tier),
        );
    }
}

fn timestamp_hour(value: &str) -> Option<u32> {
    value.get(11..13)?.parse().ok()
}

fn attempt_agent_state(state: AttemptState) -> AgentState {
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

pub fn attempt_state_word(state: AttemptState) -> &'static str {
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

#[derive(Debug, Clone, Copy, Default)]
pub struct Spend {
    micros: u64,
    priced: usize,
    unpriced: usize,
    input: u64,
    output: u64,
    missing_input: usize,
    missing_output: usize,
}

impl Spend {
    fn add(&mut self, entry: &CostEntry) {
        match entry.cost_micros {
            Some(micros) => {
                self.micros = self.micros.saturating_add(micros.value());
                self.priced = self.priced.saturating_add(1);
            }
            None => self.unpriced = self.unpriced.saturating_add(1),
        }
        match entry.input_tokens {
            Some(count) => self.input = self.input.saturating_add(count.value()),
            None => self.missing_input = self.missing_input.saturating_add(1),
        }
        match entry.output_tokens {
            Some(count) => self.output = self.output.saturating_add(count.value()),
            None => self.missing_output = self.missing_output.saturating_add(1),
        }
    }

    pub fn entries(&self) -> usize {
        self.priced + self.unpriced
    }

    pub fn text(&self) -> String {
        match (self.priced, self.unpriced) {
            (0, 0) => "-".to_owned(),
            (0, unpriced) => format!("+{unpriced}u"),
            (_, 0) => dollars(self.micros),
            (_, unpriced) => format!("{} +{unpriced}u", dollars(self.micros)),
        }
    }

    pub fn token_text(&self) -> String {
        if self.entries() == 0 {
            "-".to_owned()
        } else {
            let input = if self.missing_input == 0 {
                tokens(self.input)
            } else {
                "?".to_owned()
            };
            let output = if self.missing_output == 0 {
                tokens(self.output)
            } else {
                "?".to_owned()
            };
            format!("{input}/{output}")
        }
    }
}

pub fn cost_attempt(
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

pub fn spend_for(state: &BoardState, attempt: &AttemptId) -> Spend {
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

fn cost_totals(state: &BoardState, now: &Timestamp) -> (u64, usize, usize) {
    let mut micros = 0u64;
    let mut priced = 0usize;
    let mut unpriced = 0usize;
    for entry in state
        .costs
        .iter()
        .filter(|entry| same_utc_date(&entry.recorded_at, now))
    {
        match entry.cost_micros {
            Some(value) => {
                micros = micros.saturating_add(value.value());
                priced = priced.saturating_add(1);
            }
            None => unpriced = unpriced.saturating_add(1),
        }
    }
    (micros, priced, unpriced)
}

fn lease_for<'a>(state: &'a BoardState, attempt: &Attempt) -> Option<&'a Lease> {
    let id = attempt.worktree_lease_id.as_ref()?;
    state.leases.iter().find(|lease| &lease.id == id)
}

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

fn render_too_small(area: Rect, buf: &mut Buffer, tier: ColorTier) {
    put(buf, area, 0, 0, "GRIDWORK", bold("fg", tier));
    put(
        buf,
        area,
        1,
        1,
        &format!(
            "terminal too small: {}x{} (minimum {MIN_WIDTH}x{MIN_HEIGHT})",
            area.width, area.height
        ),
        style("warn", tier),
    );
}

fn identity(agent: &Agent) -> &str {
    match agent.role.as_deref() {
        Some(role) => short_role(role),
        None => agent
            .id
            .as_str()
            .strip_prefix("agent-")
            .unwrap_or_else(|| agent.id.as_str()),
    }
}

pub fn short_role(role: &str) -> &str {
    let base = role.strip_prefix("gw-").unwrap_or(role);
    match base {
        "orchestrator" => "orch",
        "researcher" => "rsrch",
        "architect" => "arch",
        "implementer" => "impl",
        "reviewer" => "rev",
        "auditor" => "audit",
        "general-purpose" => "general",
        "security-auditor" => "sec-audit",
        "test-automator" => "test-auto",
        "devrel-writer" => "devrel",
        other => other,
    }
}

pub fn dollars(micros: u64) -> String {
    let cents = (u128::from(micros) + 5_000) / 10_000;
    format!("${}.{:02}", cents / 100, cents % 100)
}

pub fn same_utc_date(left: &Timestamp, right: &Timestamp) -> bool {
    left.as_str()
        .get(..10)
        .is_some_and(|date| right.as_str().get(..10).is_some_and(|other| date == other))
}

pub fn tokens(count: u64) -> String {
    if count < 1_000 {
        count.to_string()
    } else {
        format!("{}.{}k", count / 1_000, (count % 1_000) / 100)
    }
}

pub(crate) fn elapsed(now: &Timestamp, started: &Timestamp) -> Option<String> {
    let seconds =
        timestamp_seconds(now.as_str())?.saturating_sub(timestamp_seconds(started.as_str())?);
    if seconds < 0 {
        return None;
    }
    let seconds = seconds as u64;
    Some(if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h{:02}", seconds / 3_600, (seconds % 3_600) / 60)
    } else {
        format!("{}d{}h", seconds / 86_400, (seconds % 86_400) / 3_600)
    })
}

fn timestamp_seconds(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || bytes.get(10) != Some(&b'T')
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
        || !value.ends_with('Z')
    {
        return None;
    }
    let number = |start: usize, end: usize| value.get(start..end)?.parse::<i64>().ok();
    let year = number(0, 4)?;
    let month = number(5, 7)?;
    let day = number(8, 10)?;
    let hour = number(11, 13)?;
    let minute = number(14, 16)?;
    let second = number(17, 19)?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=60).contains(&second)
    {
        return None;
    }
    Some(
        days_from_civil(year, month, day)
            .saturating_mul(86_400)
            .saturating_add(hour * 3_600 + minute * 60 + second),
    )
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn hhmm(timestamp: &Timestamp) -> &str {
    timestamp.as_str().get(11..16).unwrap_or(timestamp.as_str())
}

fn token(name: &str) -> &'static Token {
    SIGNAL
        .iter()
        .find(|token| token.name == name)
        .expect("the SIGNAL token inventory is pinned")
}

fn style(name: &str, tier: ColorTier) -> Style {
    theme::token_style(token(name), tier)
}

fn bold(name: &str, tier: ColorTier) -> Style {
    style(name, tier).add_modifier(Modifier::BOLD)
}

fn binding(state: AgentState) -> &'static StateBinding {
    theme::binding(match state {
        AgentState::Idle => "idle",
        AgentState::Queued => "queued",
        AgentState::Starting => "starting",
        AgentState::Running => "running",
        AgentState::Canceling => "canceling",
        AgentState::NeedsAttention => "needs_attention",
        AgentState::Blocked => "blocked",
        AgentState::Failed => "failed",
        AgentState::Done => "done",
        AgentState::Canceled => "canceled",
        AgentState::Unknown => "unknown",
    })
}

fn state_glyph(state: AgentState, glyphs: GlyphSet, phase: usize) -> char {
    let mark =
        gwk_theme::marks::mark(binding(state).mark).expect("the state mark inventory is pinned");
    theme::glyph(mark, phase, glyphs)
}

pub fn tier_badge(tier: ColorTier, glyphs: GlyphSet) -> String {
    let tier = match tier {
        ColorTier::Truecolor => "tc",
        ColorTier::Xterm256 => "256",
        ColorTier::Ansi16 => "a16",
        ColorTier::Mono => "mono",
    };
    let glyphs = match glyphs {
        GlyphSet::Unicode => "uni",
        GlyphSet::Ascii => "asc",
    };
    format!("{tier}+{glyphs}")
}

fn put(buf: &mut Buffer, area: Rect, x: u16, y: u16, text: &str, paint: Style) {
    if y >= area.height || x >= area.width {
        return;
    }
    let budget = area.width.saturating_sub(x) as usize;
    let safe = theme::safe_text(text, budget);
    buf.set_stringn(area.x + x, area.y + y, safe.as_ref(), budget, paint);
}

fn put_cell(buf: &mut Buffer, area: Rect, start: u16, end: u16, y: u16, text: &str, paint: Style) {
    if y >= area.height || start >= area.width || end <= start {
        return;
    }
    let budget = end.min(area.width).saturating_sub(start + 1) as usize;
    if budget == 0 {
        return;
    }
    let safe = theme::safe_text(text, budget);
    buf.set_stringn(area.x + start, area.y + y, safe.as_ref(), budget, paint);
}

/// A cell carrying one machine value. An over-budget value drops whole
/// behind the ruled `+` omission mark — a mid-value cut ("12d17h" painted
/// as "12d1") reads as a plausible smaller value, which the design record
/// bans outright.
fn put_value_cell(
    buf: &mut Buffer,
    area: Rect,
    start: u16,
    end: u16,
    y: u16,
    text: &str,
    paint: Style,
) {
    if y >= area.height || start >= area.width || end <= start {
        return;
    }
    let budget = end.min(area.width).saturating_sub(start + 1) as usize;
    if budget == 0 {
        return;
    }
    let (lines, omitted) = theme::safe_text_lines(text, budget, 1);
    let safe = lines.first().map_or("", String::as_str);
    if omitted {
        let mark_x = area.x + start + u16::try_from(budget).unwrap_or(u16::MAX) - 1;
        buf.set_stringn(mark_x, area.y + y, "+", 1, paint);
        return;
    }
    buf.set_stringn(area.x + start, area.y + y, safe, budget, paint);
}

/// The row's last value cell: budgets to the row edge — the phantom
/// separator gutter belongs between columns, and burning one here cut the
/// AGE cell to four cells at the 120-column design width.
fn put_tail_cell(buf: &mut Buffer, area: Rect, start: u16, y: u16, text: &str, paint: Style) {
    if y >= area.height || start >= area.width {
        return;
    }
    let budget = area.width.saturating_sub(start) as usize;
    let (lines, omitted) = theme::safe_text_lines(text, budget, 1);
    let safe = lines.first().map_or("", String::as_str);
    if omitted {
        buf.set_stringn(area.x + area.width - 1, area.y + y, "+", 1, paint);
        return;
    }
    buf.set_stringn(area.x + start, area.y + y, safe, budget, paint);
}

fn put_right(buf: &mut Buffer, area: Rect, y: u16, text: &str, paint: Style) {
    let safe = theme::safe_text(text, area.width as usize);
    let width = safe.chars().count() as u16;
    if width < area.width {
        put(buf, area, area.width - width - 1, y, safe.as_ref(), paint);
    }
}

fn put_keybar(buf: &mut Buffer, area: Rect, tier: ColorTier, wide: &str, narrow: &str) {
    let keys = if area.width < 100 { narrow } else { wide };
    put(buf, area, 0, area.height - 1, keys, style("muted", tier));
}

fn put_rule(buf: &mut Buffer, area: Rect, y: u16, tier: ColorTier) {
    put(
        buf,
        area,
        1,
        y,
        &"-".repeat(area.width.saturating_sub(2) as usize),
        style("faint", tier),
    );
}
