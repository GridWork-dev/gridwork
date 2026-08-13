//! Round 8 — the FLEET lens's unknowns block (audit findings B5 + B6).
//!
//! MOCKUP PAINTER ONLY — no production lens code. Two candidate treatments
//! for the one thing the console's FLEET lens does not do and its Board twin
//! does: state, in words, the facts the log does not carry.
//!
//! `board::agent_fleet` already builds three [`FleetUnknown`] notes — liveness,
//! engine binding, fleet size — and `render_fleet` has never called it, so on
//! the console those three facts are simply absent. The governing rule is that
//! **what is not in the log is not on the panel, and the panel says so**; the
//! open question is not whether to say it but where, because a compact lens
//! that opens with four rows of caveats has traded the estate for its
//! footnotes.
//!
//! ## The two candidates
//!
//! - **`pinned`** — the twin's own shape, ported literally: the block sits
//!   directly beneath the column header at every rung, always fully worded,
//!   above the attempt rows so it can never scroll away.
//! - **`footer`** — the block sits beside UNCLAIMED, the lens's existing
//!   "what the rows do not cover" footer, and is density-adaptive: fully
//!   worded when the frame can spare the rows, collapsed to a single
//!   subject line when it cannot.
//!
//! ## How these frames are painted
//!
//! The chrome is the REAL [`render_fleet`] over the seeded workday, so every
//! column offset, every join, and every cell budget is the shipped one — the
//! precedent is Round 3 (real `drilldown::render` inside a mocked attach
//! frame) and Round 4 (real `queue::render` inside mocked lens chrome). The
//! candidate block is then painted OVER that frame at the coordinates it
//! would occupy.
//!
//! The one thing that makes these mockups and not renders: overwriting is not
//! reflowing. A shipped block pushes the rows beneath it down; here it covers
//! them. The cost each candidate charges is therefore stated as a row count in
//! the caption rather than drawn, and the ruled winner's real reflow lands in
//! the `console_fleet` goldens.
//!
//! ## The scenario
//!
//! The seeded workday fires only one of the three notes, which is the wrong
//! frame to choose a layout against. This round reads it as a PARTIAL page
//! with one engine session missing:
//!
//! - `complete = false` — the read stopped short of the last projection page,
//!   so every count on the lens is a floor rather than a total.
//! - `es-pty-impl` removed — `at-pty-impl` is Running with no engine session
//!   on the page at all, which is the "engine binding" note. Its cost is
//!   unaffected: `ce-08` reaches that attempt through the dispatch node
//!   `d-pty-recon`, not through the session.
//!
//! Both are states the wire permits and the shipped console has no rendering
//! for. Three notes is also the widest the block gets today, so a layout that
//! holds here holds.

mod common;

#[path = "mockups/shared.rs"]
mod shared;

use common::{assert_matches_golden, dump_frame};
use gwk_domain::ids::Timestamp;
use gwk_theme::marks::GlyphSet;
use gwk_theme::tier::ColorTier;
use gwk_tui::board::{BoardState, BoardView, FleetUnknown, agent_fleet};
use gwk_tui::console::{FleetContext, LoadState, render_fleet};
use gwk_tui::input::HitMap;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use shared::{bold, put, style};

const NOW: &str = "2026-08-11T17:30:00Z";

/// The partial-read workday described in the module doc.
fn scenario() -> BoardState {
    let mut state = common::estate::estate_board_state(BoardView::Fleet);
    state.complete = false;
    state
        .sessions
        .retain(|session| session.id.as_str() != "es-pty-impl");
    state
}

/// The real lens, so the candidate block is judged against real cell budgets.
fn paint_chrome(
    area: Rect,
    buf: &mut Buffer,
    state: &BoardState,
    tier: ColorTier,
    glyphs: GlyphSet,
) {
    let context = FleetContext {
        now: Timestamp::new(NOW),
        load: LoadState::Ready,
    };
    let mut hits = HitMap::new();
    render_fleet(area, buf, state, &context, None, tier, glyphs, &mut hits);
}

/// The row the shipped frame draws its rule on, found by reading the painted
/// buffer rather than re-deriving the lens's layout arithmetic — a mockup that
/// computed the position itself could place the block somewhere the real frame
/// never has.
fn rule_row(buf: &Buffer, area: Rect) -> u16 {
    (0..area.height)
        .find(|y| (1..4).all(|x| buf[(area.x + x, area.y + *y)].symbol() == "-"))
        .expect("the fleet frame draws a rule above its chart")
}

/// Blank the rows the block claims before painting into them. A shipped block
/// reflows what sits beneath it; a mockup that painted over live text would
/// read as a rendering defect instead of as a layout proposal.
fn clear_rows(buf: &mut Buffer, area: Rect, top: u16, rows: u16, tier: ColorTier) {
    let blank = " ".repeat(area.width as usize);
    for row in 0..rows {
        put(
            buf,
            area,
            0,
            top.saturating_add(row),
            &blank,
            style("fg", tier),
        );
    }
}

/// The fully worded block: a heading that counts, then one note per row.
fn put_full_block(buf: &mut Buffer, area: Rect, top: u16, notes: &[FleetUnknown], tier: ColorTier) {
    clear_rows(buf, area, top, notes.len() as u16 + 1, tier);
    put(
        buf,
        area,
        2,
        top,
        &format!(
            "UNKNOWN  {} fact{} not in the log",
            notes.len(),
            if notes.len() == 1 { "" } else { "s" }
        ),
        bold("fg", tier),
    );
    for (index, note) in notes.iter().enumerate() {
        put(
            buf,
            area,
            4,
            top.saturating_add(index as u16 + 1),
            &format!("{}: {}", note.subject, note.why),
            style("muted", tier),
        );
    }
}

/// The one-row form: the `why` clauses drop, the subjects do not. A block that
/// degraded to a bare count would name the number of things it was not saying.
fn put_compact_block(
    buf: &mut Buffer,
    area: Rect,
    top: u16,
    notes: &[FleetUnknown],
    tier: ColorTier,
) {
    clear_rows(buf, area, top, 1, tier);
    let subjects = notes
        .iter()
        .map(|note| note.subject)
        .collect::<Vec<_>>()
        .join(", ");
    put(
        buf,
        area,
        2,
        top,
        &format!("UNKNOWN  {subjects}"),
        bold("fg", tier),
    );
}

/// A caption stating what the candidate costs, since an overwriting mockup
/// cannot show the rows it would have displaced.
fn put_caption(buf: &mut Buffer, area: Rect, text: &str, tier: ColorTier) {
    put(buf, area, 2, area.height - 2, text, style("warn", tier));
}

fn paint_pinned(area: Rect, buf: &mut Buffer, tier: ColorTier, glyphs: GlyphSet) {
    let state = scenario();
    paint_chrome(area, buf, &state, tier, glyphs);
    let notes = agent_fleet(&state).unknowns;
    // Row 3 is the first attempt row: the block takes the top of the list at
    // every rung, exactly as the Board twin's pinned block does.
    put_full_block(buf, area, 3, &notes, tier);
    put_caption(
        buf,
        area,
        &format!(
            "candidate pinned -- block always {} rows; attempt rows {} -> {}",
            notes.len() + 1,
            area.height - 13,
            area.height as usize - 13 - (notes.len() + 1),
        ),
        tier,
    );
}

fn paint_footer(area: Rect, buf: &mut Buffer, tier: ColorTier, glyphs: GlyphSet) {
    let state = scenario();
    paint_chrome(area, buf, &state, tier, glyphs);
    let notes = agent_fleet(&state).unknowns;
    let top = rule_row(buf, area);
    // Fully worded only while at least eight attempt rows survive it; the
    // floor rung is where that stops being true.
    let full = area.height as usize >= 13 + (notes.len() + 1) + 8;
    if full {
        put_full_block(buf, area, top, &notes, tier);
    } else {
        put_compact_block(buf, area, top, &notes, tier);
    }
    put_caption(
        buf,
        area,
        &format!(
            "candidate footer -- block {} rows; attempt rows {} -> {}",
            if full { notes.len() + 1 } else { 1 },
            area.height - 13,
            area.height as usize - 13 - if full { notes.len() + 1 } else { 1 },
        ),
        tier,
    );
}

fn check(
    candidate: &str,
    width: u16,
    height: u16,
    tier: ColorTier,
    glyphs: GlyphSet,
    paint: fn(Rect, &mut Buffer, ColorTier, GlyphSet),
) {
    let name = format!(
        "mock-unknowns-{candidate}-{width}x{height}-{}-{}",
        tier.as_str(),
        match glyphs {
            GlyphSet::Unicode => "unicode",
            GlyphSet::Ascii => "ascii",
        }
    );
    assert_matches_golden(&name, &dump_frame(width, height, tier, glyphs, paint));
}

#[test]
fn pinned_at_120x40() {
    check(
        "pinned",
        120,
        40,
        ColorTier::Truecolor,
        GlyphSet::Unicode,
        paint_pinned,
    );
}

#[test]
fn pinned_at_the_80x24_floor() {
    check(
        "pinned",
        80,
        24,
        ColorTier::Truecolor,
        GlyphSet::Unicode,
        paint_pinned,
    );
}

#[test]
fn footer_at_120x40() {
    check(
        "footer",
        120,
        40,
        ColorTier::Truecolor,
        GlyphSet::Unicode,
        paint_footer,
    );
}

#[test]
fn footer_at_the_100x30_snapshot_rung() {
    check(
        "footer",
        100,
        30,
        ColorTier::Truecolor,
        GlyphSet::Unicode,
        paint_footer,
    );
}

#[test]
fn footer_at_the_80x24_floor() {
    check(
        "footer",
        80,
        24,
        ColorTier::Truecolor,
        GlyphSet::Unicode,
        paint_footer,
    );
}

/// The degraded rung, which is where a treatment leaning on colour disappears:
/// every token these notes use resolves to the terminal's own foreground at
/// Mono, so the words are the whole signal or there is no signal.
#[test]
fn footer_degraded_mono_ascii() {
    check(
        "footer",
        120,
        40,
        ColorTier::Mono,
        GlyphSet::Ascii,
        paint_footer,
    );
}

#[test]
fn pinned_degraded_mono_ascii() {
    check(
        "pinned",
        120,
        40,
        ColorTier::Mono,
        GlyphSet::Ascii,
        paint_pinned,
    );
}
