//! Round 1 — Hall at-rest, the RULED build spec (grill G5 enriched ambient;
//! the picker chose `inline` over `stacked`, 2026-08-11).
//!
//! MOCKUP PAINTER ONLY. Nothing here is production lens code; this test
//! hand-paints the ruled layout over the seeded workday estate and blesses
//! it as `mock-*` goldens — the spec the EXECUTE lane builds against. The
//! `mock-` prefix keeps these disjoint from the harness-owned `seed-*`
//! files. The losing `stacked` candidate (compact glyph field + a
//! hot-callout line) is retired; its frames are in this branch's history at
//! `f1a858e`.
//!
//! The ruled shape, two rows per district:
//!
//! ```text
//!   KERNEL  !1
//!   pty  <glyph> impl 7h50   ! rev 7h42     blob  <glyph> arch 6h05   . rust-pro
//! ```
//!
//! - Row 0 is a vitals header; the last row is the keybar. Both always
//!   paint, and both shorten rather than vanishing.
//! - Focus reads as a `>` prefix on the district heading, never colour
//!   alone, so it survives Mono where `focus` is reverse-video-only.
//! - Identity is role TEXT beside the state glyph, not a WHO glyph: the
//!   ratified identity inventory is closed at 7 marks and cannot name the
//!   real `gw-*` taxonomy. A roleless agent falls back to its own id with
//!   the namespace stripped rather than a placeholder — round 1 exposed
//!   `? -` as unreadable, and at Ascii that dash also collided with the
//!   `running` mark's own `-`.
//! - Elapsed drops first under width pressure (see the 80x24 golden); role
//!   text would follow at a denser rung this round does not yet cover.
//!
//! Fixture-derived numbers: run 4 = 2 Running + 1 Starting + 1 Canceling;
//! !2 = the two unresolved attention items; $5.30 = the ten priced
//! `cost_micros` entries; as-of 221 = the seeded watermark; 17:30 = the
//! seeded clock. Elapsed strings are hand-assigned from the seeded day
//! because `FrameInput` carries no timestamps at all — the live path
//! hardcodes the literal string "live". Carrying real start times into
//! `FrameInput` is a build-spec requirement this round records.

mod common;

#[path = "mockups/shared.rs"]
mod shared;

use common::{assert_matches_golden, dump_frame};
use gwk_theme::marks::GlyphSet;
use gwk_theme::tier::ColorTier;
use gwk_tui::hall::{Agent, District, FrameInput};
use gwk_tui::theme::state_style;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use shared::{
    binding, bold, put, put_keybar, put_right, short_role, state_glyph, style, tier_badge,
};

/// Identity text for one agent. A roleless agent falls back to its own id
/// with the `agent-` namespace stripped — an id is always present, always
/// retypable, and always more useful than a placeholder.
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

/// Elapsed-time text, hand-assigned from the seeded day because
/// `FrameInput` carries no timestamps. `None` = not a live agent, so no
/// duration is owed. The three agents whose ids match a seeded attempt
/// (`pty-impl`, `pty-review`, `tui-impl`) carry that attempt's real age;
/// the rest are hand-set, because the fixture's Hall agents are a separate
/// hand-built set rather than a projection of its attempts — a coherence
/// gap worth closing in the harness, noted in DESIGN-NOTES.
fn elapsed(agent: &Agent) -> Option<&'static str> {
    match agent.id.as_str() {
        "agent-pty-impl" => Some("7h50"),
        "agent-pty-review" => Some("7h42"),
        "agent-blob-arch" => Some("6h05"),
        "agent-tui-impl" => Some("7h30"),
        "agent-tui-audit" => Some("54s"),
        "agent-tui-cancel" => Some("5h28"),
        "agent-deploy-arch" => Some("2h10"),
        "agent-audit-sec" => Some("1h10"),
        _ => None,
    }
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

fn paint_header(buf: &mut Buffer, area: Rect, tier: ColorTier, glyphs: GlyphSet, compact: bool) {
    put(buf, area, 1, 0, "GRIDWORK", bold("gws_fg", tier));
    put(buf, area, 11, 0, "run 4", style("gws_hue", tier));
    put(buf, area, 18, 0, "!2", style("gws_warn", tier));
    put(buf, area, 22, 0, "$5.30 today", style("gws_fg", tier));

    let badge = tier_badge(tier, glyphs);
    let right = if compact {
        format!("{badge}  17:30")
    } else {
        format!("tier {badge}  as-of 221  17:30")
    };
    put_right(buf, area, 0, &right, style("gws_muted", tier));
}

fn paint_heading(
    buf: &mut Buffer,
    area: Rect,
    y: u16,
    tier: ColorTier,
    input: &FrameInput,
    district: &District,
) {
    let focused = is_focused(input, district);
    let text = format!(
        "{} {}",
        if focused { ">" } else { " " },
        district.label.to_uppercase()
    );
    let heading = if focused {
        bold("gws_focus", tier)
    } else {
        bold("gws_fg", tier)
    };
    put(buf, area, 0, y, &text, heading);

    let mut x = text.chars().count() as u16 + 2;
    let attention = unresolved_attention(input, district);
    if attention > 0 {
        let badge = format!("!{attention}");
        put(buf, area, x, y, &badge, style("gws_warn", tier));
        x += badge.chars().count() as u16 + 2;
    }
    if district.aged_done > 0 {
        let done = format!("+{} done", district.aged_done);
        put(buf, area, x, y, &done, style("gws_muted", tier));
    }
}

fn paint_hall(area: Rect, buf: &mut Buffer, tier: ColorTier, glyphs: GlyphSet) {
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
            put(buf, area, x, y, &station.label, style("gws_muted", tier));
            x += station.label.chars().count() as u16 + 2;

            for agent in &station.agents {
                let mark = state_glyph(agent.state, glyphs);
                let cell = match elapsed(agent) {
                    Some(elapsed) if !compact => format!("{mark} {} {elapsed}", identity(agent)),
                    _ => format!("{mark} {}", identity(agent)),
                };
                let paint = state_style(binding(agent.state), tier);
                put(buf, area, x, y, &cell, paint);
                x += cell.chars().count() as u16 + 3;
            }
            x += 2;
        }
        y += 2;
    }

    put_keybar(
        buf,
        area,
        tier,
        " : go   / filter   enter open   j/k district   [ ] page   m motion   q quit",
        " : go   / filter   enter open   j/k   q quit",
    );
    if area.width >= 100 {
        put_right(
            buf,
            area,
            area.height - 1,
            "4 districts  15 agents",
            style("gws_muted", tier),
        );
    }
}

fn check(width: u16, height: u16, tier: ColorTier, glyphs: GlyphSet) {
    let glyph_name = match glyphs {
        GlyphSet::Unicode => "unicode",
        GlyphSet::Ascii => "ascii",
    };
    let name = format!(
        "mock-hall-inline-{width}x{height}-{}-{glyph_name}",
        tier.as_str()
    );
    assert_matches_golden(&name, &dump_frame(width, height, tier, glyphs, paint_hall));
}

#[test]
fn hall_at_120x40() {
    check(120, 40, ColorTier::Truecolor, GlyphSet::Unicode);
}

#[test]
fn hall_at_80x24() {
    check(80, 24, ColorTier::Truecolor, GlyphSet::Unicode);
}

#[test]
fn hall_degraded_ansi16() {
    check(120, 40, ColorTier::Ansi16, GlyphSet::Unicode);
}

#[test]
fn hall_degraded_mono_ascii() {
    check(120, 40, ColorTier::Mono, GlyphSet::Ascii);
}
