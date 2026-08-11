//! Shared chrome for the `mock_*` design rounds.
//!
//! Pulled in with `#[path = "mockups/shared.rs"] mod shared;` — a
//! subdirectory of `tests/` is never auto-compiled as its own test binary,
//! the same mechanism `tests/common/` relies on. It lives here rather than
//! in `tests/common/` because that directory belongs to the harness lane;
//! this is design-round scaffolding.
//!
//! Everything here is a candidate for the ruled build spec, not production
//! code: the token/mark lookups mirror what a real lens does through
//! `gwk_tui::theme`, and the layout helpers are deliberately the simplest
//! thing that paints an honest frame.
//!
//! Compiled into every mock binary that declares it, so not every helper is
//! used by every round — same reason `tests/common/mod.rs` carries this.
#![allow(dead_code)]

use gwk_theme::marks::{GlyphSet, STATES, StateBinding};
use gwk_theme::tier::ColorTier;
use gwk_theme::{SIGNAL, Token};
use gwk_tui::hall::AgentState;
use gwk_tui::theme::{glyph, token_style};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

// ---------------------------------------------------------------------------
// Theme access
// ---------------------------------------------------------------------------

pub fn token(name: &str) -> &'static Token {
    SIGNAL
        .iter()
        .find(|token| token.name == name)
        .expect("ratified token")
}

pub fn style(name: &str, tier: ColorTier) -> Style {
    token_style(token(name), tier)
}

pub fn bold(name: &str, tier: ColorTier) -> Style {
    style(name, tier).add_modifier(Modifier::BOLD)
}

pub fn binding(state: AgentState) -> &'static StateBinding {
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

pub fn state_glyph(state: AgentState, glyphs: GlyphSet) -> char {
    let mark = gwk_theme::marks::mark(binding(state).mark).expect("ratified mark");
    glyph(mark, 0, glyphs)
}

/// The resolved capability badge. The shipped console resolves both tiers
/// silently — an operator downgraded to Mono over ssh, or to Ascii by a
/// failed glyph probe, has no way to learn why the screen looks wrong.
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

// ---------------------------------------------------------------------------
// Identity + money text
// ---------------------------------------------------------------------------

/// The ruled role-to-label mapping (round 1): strip the `gw-` lane prefix,
/// then shorten the long canonical names. An unrecognised role passes
/// through as-is — the role vocabulary is open, so the fallback must
/// render, never panic.
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

/// Micros to a dollar string, rounded half-up. `CostMicros` is a u64 on the
/// wire (a canonical decimal *string*, never a JSON number) — money never
/// becomes a float here either.
///
/// Rounding is per-value, so a column of rounded rows can differ from a
/// rounded total by a cent or two. Totals are always folded from unrounded
/// micros and are the authority; the caveat is recorded in DESIGN-NOTES
/// rather than papered over by rounding the total to match its rows.
pub fn dollars(micros: u64) -> String {
    let cents = (micros + 5_000) / 10_000;
    format!("${}.{:02}", cents / 100, cents % 100)
}

/// Thousands-suffixed token counts: 15000 -> `15.0k`.
pub fn tokens(count: u64) -> String {
    if count < 1_000 {
        return count.to_string();
    }
    format!("{}.{}k", count / 1_000, (count % 1_000) / 100)
}

// ---------------------------------------------------------------------------
// Painting primitives
// ---------------------------------------------------------------------------

/// Clip-and-paint one run of text. All mock text is ASCII-safe by
/// construction apart from the ratified marks, so a cell budget clip is the
/// only discipline these frames need.
pub fn put(buf: &mut Buffer, area: Rect, x: u16, y: u16, text: &str, style: Style) {
    if y >= area.height || x >= area.width {
        return;
    }
    let budget = (area.width - x) as usize;
    let clipped: String = text.chars().take(budget).collect();
    buf.set_string(area.x + x, area.y + y, clipped, style);
}

pub fn put_right(buf: &mut Buffer, area: Rect, y: u16, text: &str, style: Style) {
    let width = text.chars().count() as u16;
    if width < area.width {
        put(buf, area, area.width - width - 1, y, text, style);
    }
}

/// A row of column cells at fixed offsets.
pub fn put_columns(buf: &mut Buffer, area: Rect, y: u16, columns: &[(u16, &str)], style: Style) {
    for (x, text) in columns {
        put(buf, area, *x, y, text, style);
    }
}

/// A full-width rule. `-` is the only structural character that is both
/// ASCII and admissible at every glyph tier; the theme's elevation tokens
/// are never-a-colour, so a rule is how a mock expresses a boundary.
pub fn put_rule(buf: &mut Buffer, area: Rect, y: u16, tier: ColorTier) {
    let rule: String = "-".repeat(area.width.saturating_sub(2) as usize);
    put(buf, area, 1, y, &rule, style("faint", tier));
}

/// The keybar shortens rather than vanishing — the one behaviour the
/// shipped status line gets backwards (it drops the whole hint block the
/// instant it does not fit).
pub fn put_keybar(buf: &mut Buffer, area: Rect, tier: ColorTier, wide: &str, narrow: &str) {
    let compact = area.width < 100;
    let keys = if compact { narrow } else { wide };
    put(buf, area, 0, area.height - 1, keys, style("muted", tier));
}
