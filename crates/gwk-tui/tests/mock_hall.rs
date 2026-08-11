//! Mockup round 1 — Hall at-rest richness (grill ruling G5: enriched ambient).
//!
//! MOCKUP PAINTERS ONLY. Nothing here is production lens code; these tests
//! hand-paint candidate layouts over the seeded workday estate and bless
//! them as `mock-*` goldens — the picker excerpts and, once ruled, the build
//! spec for the EXECUTE lane. The `mock-` prefix keeps this suite's goldens
//! disjoint from the harness-owned `seed-*` files.
//!
//! Two candidates, same ruled direction (real elapsed times, painted focus,
//! one vitals header, keybar honesty):
//!
//! - `inline`  — 2-row districts: heading, then stations and agents on one
//!   line (`station  <state-glyph> role elapsed`). Identity is the role
//!   TEXT at the widest rung; glyph pairs return under density.
//! - `stacked` — 3/4-row districts: heading, station labels, a compact
//!   state-glyph field (as shipped today), plus a hot-callout line naming
//!   only needs-attention/blocked/failed/unknown agents with role + state
//!   word + elapsed.
//!
//! Chrome discipline: every non-mark glyph in these frames is plain ASCII
//! (`>`, `!`, `+`, `$`, `-`) — geometric/arrow codepoints are mostly
//! East-Asian-Width Ambiguous and the theme's own admission doctrine treats
//! ambiguous-width glyphs as shear risks. State glyphs come exclusively from
//! the ratified mark inventory via `theme::glyph`. This doubles as the
//! mono/ascii honesty rule: focus reads as a `>` prefix (never color alone).
//!
//! Vitals in the header are hand-folded from the same seeded day
//! (`tests/common/estate.rs`): run 4 = 2 Running + 1 Starting + 1 Canceling;
//! attn 2 = the two unresolved attention items; $5.30 = the sum of the ten
//! priced `cost_micros` entries (5,300,000 micros); as-of 221 = the seeded
//! watermark; 17:30 = the seeded clock. Elapsed strings are hand-assigned
//! from the day's timeline because `FrameInput` carries no timestamps —
//! carrying real ones is a build-spec requirement this round records.

mod common;

use common::{assert_matches_golden, dump_frame};
use gwk_theme::marks::{GlyphSet, STATES, StateBinding};
use gwk_theme::tier::ColorTier;
use gwk_theme::{SIGNAL, Token};
use gwk_tui::hall::{Agent, AgentState, District, FrameInput};
use gwk_tui::theme::{glyph, state_style, token_style};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

// ---------------------------------------------------------------------------
// Shared chrome
// ---------------------------------------------------------------------------

fn token(name: &str) -> &'static Token {
    SIGNAL
        .iter()
        .find(|token| token.name == name)
        .expect("ratified token")
}

fn binding(state: AgentState) -> &'static StateBinding {
    let name = match state {
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
    };
    STATES
        .iter()
        .find(|binding| binding.name == name)
        .expect("ratified state binding")
}

fn state_glyph(state: AgentState, glyphs: GlyphSet) -> char {
    let binding = binding(state);
    let mark = gwk_theme::marks::mark(binding.mark).expect("ratified mark");
    glyph(mark, 0, glyphs)
}

/// Clip-and-paint one run of text. All fixture text is ASCII-safe by
/// construction; the budget clip is the only discipline this mock needs.
fn put(buf: &mut Buffer, area: Rect, x: u16, y: u16, text: &str, style: Style) {
    if y >= area.height || x >= area.width {
        return;
    }
    let budget = (area.width - x) as usize;
    let clipped: String = text.chars().take(budget).collect();
    buf.set_string(area.x + x, area.y + y, clipped, style);
}

fn put_right(buf: &mut Buffer, area: Rect, y: u16, text: &str, style: Style) {
    let width = text.chars().count() as u16;
    if width < area.width {
        put(buf, area, area.width - width - 1, y, text, style);
    }
}

/// Hand-assigned enrichment for the fifteen seeded agents: short role label
/// plus elapsed-time text sourced from the seeded day's timestamps.
fn enrich(agent: &Agent) -> (&'static str, &'static str) {
    match agent.id.as_str() {
        "agent-pty-impl" => ("impl", "7h50"),
        "agent-pty-review" => ("rev", "7h00"),
        "agent-blob-arch" => ("arch", "6h05"),
        "agent-blob-idle" => ("rust-pro", "-"),
        "agent-tui-impl" => ("impl", "7h30"),
        "agent-tui-audit" => ("audit", "54s"),
        "agent-tui-general" => ("general", "-"),
        "agent-tui-cancel" => ("rev", "5h28"),
        "agent-docs-writer" => ("devrel", "-"),
        "agent-docs-none" => ("-", "-"),
        "agent-deploy-arch" => ("arch", "2h10"),
        "agent-ci-test" => ("test-auto", "-"),
        "agent-ci-watch" => ("rsrch", "-"),
        "agent-audit-sec" => ("sec-audit", "1h10"),
        "agent-audit-done" => ("audit", "-"),
        other => panic!("unseeded agent {other}"),
    }
}

fn state_word(state: AgentState) -> &'static str {
    match state {
        AgentState::NeedsAttention => "attn",
        AgentState::Blocked => "blocked",
        AgentState::Failed => "failed",
        AgentState::Unknown => "unknown",
        _ => "",
    }
}

fn is_hot(state: AgentState) -> bool {
    matches!(
        state,
        AgentState::NeedsAttention | AgentState::Blocked | AgentState::Failed | AgentState::Unknown
    )
}

fn unresolved_attention(input: &FrameInput, district: &District) -> usize {
    input
        .attention
        .iter()
        .filter(|attention| attention.unresolved && attention.district == district.id)
        .count()
}

fn is_focused(input: &FrameInput, district: &District) -> bool {
    input
        .focus
        .as_ref()
        .is_some_and(|focus| focus.district == district.id)
}

/// Row 0: the single vitals header the G5 ruling asks for. Left = estate
/// pulse, right = focus target, tier+glyph state (silent degradation made
/// visible), watermark, clock.
fn tier_badge(tier: ColorTier, glyphs: GlyphSet) -> String {
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

fn paint_header(buf: &mut Buffer, area: Rect, tier: ColorTier, glyphs: GlyphSet, compact: bool) {
    let bold = token_style(token("fg"), tier).add_modifier(Modifier::BOLD);
    let muted = token_style(token("muted"), tier);
    let warn = token_style(token("warn"), tier);

    put(buf, area, 1, 0, "GRIDWORK", bold);
    put(buf, area, 11, 0, "run 4", token_style(token("hue"), tier));
    put(buf, area, 18, 0, "!2", warn);
    put(
        buf,
        area,
        22,
        0,
        "$5.30 today",
        token_style(token("fg"), tier),
    );
    let badge = tier_badge(tier, glyphs);
    let right = if compact {
        format!("{badge}  17:30")
    } else {
        format!("tier {badge}  as-of 221  17:30")
    };
    put_right(buf, area, 0, &right, muted);
}

fn paint_keybar(buf: &mut Buffer, area: Rect, tier: ColorTier, compact: bool) {
    let muted = token_style(token("muted"), tier);
    let y = area.height - 1;
    let keys = if compact {
        " : go   / filter   enter open   j/k   q quit"
    } else {
        " : go   / filter   enter open   j/k district   [ ] page   m motion   q quit"
    };
    put(buf, area, 0, y, keys, muted);
    if !compact {
        put_right(buf, area, y, "4 districts  15 agents", muted);
    }
}

fn heading_line(input: &FrameInput, district: &District) -> (String, bool) {
    let focused = is_focused(input, district);
    let mut text = String::new();
    text.push_str(if focused { "> " } else { "  " });
    text.push_str(&district.label.to_uppercase());
    (text, focused)
}

fn paint_heading(
    buf: &mut Buffer,
    area: Rect,
    y: u16,
    tier: ColorTier,
    input: &FrameInput,
    district: &District,
) {
    let (text, focused) = heading_line(input, district);
    let style = if focused {
        token_style(token("focus"), tier).add_modifier(Modifier::BOLD)
    } else {
        token_style(token("fg"), tier).add_modifier(Modifier::BOLD)
    };
    put(buf, area, 0, y, &text, style);

    let mut x = text.chars().count() as u16 + 2;
    let attention = unresolved_attention(input, district);
    if attention > 0 {
        let badge = format!("!{attention}");
        put(buf, area, x, y, &badge, token_style(token("warn"), tier));
        x += badge.chars().count() as u16 + 2;
    }
    if district.aged_done > 0 {
        let done = format!("+{} done", district.aged_done);
        put(buf, area, x, y, &done, token_style(token("muted"), tier));
    }
}

// ---------------------------------------------------------------------------
// Candidate A — inline: 2-row districts, identity as role text
// ---------------------------------------------------------------------------

fn paint_inline(area: Rect, buf: &mut Buffer, tier: ColorTier, glyphs: GlyphSet) {
    let input = common::estate::estate_frame_input();
    let compact = area.width < 100;
    paint_header(buf, area, tier, glyphs, compact);

    let mut y = 2;
    for district in &input.districts {
        if y + 2 >= area.height {
            break;
        }
        paint_heading(buf, area, y, tier, &input, district);
        y += 1;

        let mut x = 2u16;
        for station in &district.stations {
            put(
                buf,
                area,
                x,
                y,
                &station.label,
                token_style(token("muted"), tier),
            );
            x += station.label.chars().count() as u16 + 2;
            for agent in &station.agents {
                let (role, elapsed) = enrich(agent);
                let style = state_style(binding(agent.state), tier);
                let cell = if compact || elapsed == "-" {
                    format!("{} {}", state_glyph(agent.state, glyphs), role)
                } else {
                    format!("{} {} {}", state_glyph(agent.state, glyphs), role, elapsed)
                };
                put(buf, area, x, y, &cell, style);
                x += cell.chars().count() as u16 + 3;
            }
            x += 2;
        }
        y += 2;
    }

    paint_keybar(buf, area, tier, compact);
}

// ---------------------------------------------------------------------------
// Candidate B — stacked: compact glyph field + hot callouts
// ---------------------------------------------------------------------------

fn paint_stacked(area: Rect, buf: &mut Buffer, tier: ColorTier, glyphs: GlyphSet) {
    let input = common::estate::estate_frame_input();
    let compact = area.width < 100;
    paint_header(buf, area, tier, glyphs, compact);

    let mut y = 2;
    for district in &input.districts {
        if y + 3 >= area.height {
            break;
        }
        paint_heading(buf, area, y, tier, &input, district);

        // Station labels and the compact state-glyph field, column-aligned.
        let mut x = 2u16;
        for station in &district.stations {
            put(
                buf,
                area,
                x,
                y + 1,
                &station.label,
                token_style(token("muted"), tier),
            );
            let mut glyph_x = x;
            for agent in &station.agents {
                let cell = state_glyph(agent.state, glyphs).to_string();
                put(
                    buf,
                    area,
                    glyph_x,
                    y + 2,
                    &cell,
                    state_style(binding(agent.state), tier),
                );
                glyph_x += 2;
            }
            let station_width =
                (station.label.chars().count() as u16).max(station.agents.len() as u16 * 2) + 4;
            x += station_width;
        }
        y += 3;

        // Hot callouts: only agents that owe the operator a look.
        let hot: Vec<&Agent> = district
            .stations
            .iter()
            .flat_map(|station| &station.agents)
            .filter(|agent| is_hot(agent.state))
            .collect();
        if !hot.is_empty() {
            let mut x = 2u16;
            for agent in hot {
                let (role, elapsed) = enrich(agent);
                let word = state_word(agent.state);
                let text = if elapsed == "-" {
                    format!("! {role} {word}")
                } else {
                    format!("! {role} {word} {elapsed}")
                };
                put(
                    buf,
                    area,
                    x,
                    y,
                    &text,
                    state_style(binding(agent.state), tier),
                );
                x += text.chars().count() as u16 + 3;
            }
            y += 1;
        }
        y += 1;
    }

    paint_keybar(buf, area, tier, compact);
}

// ---------------------------------------------------------------------------
// The round's golden matrix
// ---------------------------------------------------------------------------

type Painter = fn(Rect, &mut Buffer, ColorTier, GlyphSet);

fn check(candidate: &str, width: u16, height: u16, tier: ColorTier, glyphs: GlyphSet) {
    let painter: Painter = match candidate {
        "inline" => paint_inline,
        "stacked" => paint_stacked,
        other => panic!("unknown candidate {other}"),
    };
    let tier_name = tier.as_str();
    let glyph_name = match glyphs {
        GlyphSet::Unicode => "unicode",
        GlyphSet::Ascii => "ascii",
    };
    let name = format!("mock-hall-{candidate}-{width}x{height}-{tier_name}-{glyph_name}");
    let rendered = dump_frame(width, height, tier, glyphs, painter);
    assert_matches_golden(&name, &rendered);
}

#[test]
fn inline_at_120x40() {
    check("inline", 120, 40, ColorTier::Truecolor, GlyphSet::Unicode);
}

#[test]
fn inline_at_80x24() {
    check("inline", 80, 24, ColorTier::Truecolor, GlyphSet::Unicode);
}

#[test]
fn inline_degraded_mono_ascii() {
    check("inline", 120, 40, ColorTier::Mono, GlyphSet::Ascii);
}

#[test]
fn stacked_at_120x40() {
    check("stacked", 120, 40, ColorTier::Truecolor, GlyphSet::Unicode);
}

#[test]
fn stacked_at_80x24() {
    check("stacked", 80, 24, ColorTier::Truecolor, GlyphSet::Unicode);
}

#[test]
fn stacked_degraded_mono_ascii() {
    check("stacked", 120, 40, ColorTier::Mono, GlyphSet::Ascii);
}
