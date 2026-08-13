//! Round 8 — the FLEET lens's unknowns block · RULED `footer` (picker,
//! 2026-08-13). Audit findings B5 + B6.
//!
//! `board::agent_fleet` builds three `FleetUnknown` notes — liveness, engine
//! binding, fleet size — and `render_fleet` had never called it, so on the
//! console those three facts were simply absent. The governing rule is that
//! **what is not in the log is not on the panel, and the panel says so**; the
//! question this round answered is not whether to say it but where, because a
//! compact lens that opens with four rows of caveats has traded the estate for
//! its footnotes.
//!
//! ## The picker
//!
//! - **`pinned`** — the Board twin's own shape ported literally: the block
//!   directly beneath the column header at every rung, always fully worded.
//! - **`footer`** — beside UNCLAIMED, the lens's existing "what the rows do
//!   not cover" block, and density-adaptive: fully worded while the frame can
//!   spare the rows, one subject line when it cannot.
//!
//! **`footer` won.** At the 80x24 floor `pinned` charged four of eleven
//! attempt rows and displaced the HEAD of the list — `at-pty-impl` among them,
//! which is the running attempt the engine-binding note is about. A caveat
//! that hides its own subject is worse than a terse one. `footer` charges one
//! row there and four at 120x40, where they are free. The losing frames are in
//! branch history at `c803a6c`; its goldens are retired so the suite carries
//! no dead maintenance.
//!
//! The pinned candidate's one real argument — unknowns must not scroll away —
//! does not bind here: `render_fleet` is a fixed window with a `+N more`
//! notice, not a scrolling list, so a footer is as pinned as a header.
//!
//! ## What these goldens are
//!
//! The REAL `render_fleet`, not a painter. The picker's overlay mockups did
//! their job and are gone; these are the ruled frames, carrying the real
//! reflow an overlay could not show.
//!
//! ## The scenarios
//!
//! - `footer` — the seeded workday read as a PARTIAL page with `es-pty-impl`
//!   absent, so all three notes fire at once and every count on the lens is a
//!   floor. `at-pty-impl` is then Running with no engine session on the page:
//!   the `NOEND` cell that used to read `-` reads `?`. Its cost is unaffected,
//!   because `ce-08` reaches that attempt through the dispatch node
//!   `d-pty-recon` rather than through the session.
//! - `seeded` — the workday exactly as seeded: a complete read, one note, and
//!   no `at least` anywhere. The frame a reader sees most often.
//! - `clean` — every session ended, on a complete page. No note is true, so
//!   there is no block at all: the treatment is evidence-driven chrome, not a
//!   standing disclaimer that teaches the eye to skip it.

mod common;

use common::{assert_matches_golden, dump_frame};
use gwk_domain::ids::Timestamp;
use gwk_theme::marks::GlyphSet;
use gwk_theme::tier::ColorTier;
use gwk_tui::board::{BoardState, BoardView};
use gwk_tui::console::{FleetContext, LoadState, render_fleet};
use gwk_tui::input::HitMap;

const NOW: &str = "2026-08-11T17:30:00Z";

fn seeded() -> BoardState {
    common::estate::estate_board_state(BoardView::Fleet)
}

/// The partial read with a running attempt whose engine session is off the
/// page — all three notes at once.
fn partial() -> BoardState {
    let mut state = seeded();
    state.complete = false;
    state
        .sessions
        .retain(|session| session.id.as_str() != "es-pty-impl");
    state
}

/// A complete page on which every session recorded an end. Nothing is unknown,
/// so nothing is claimed to be.
fn clean() -> BoardState {
    let mut state = seeded();
    for session in &mut state.sessions {
        if session.ended_at.is_none() {
            session.ended_at = Some(Timestamp::new("2026-08-11T17:00:00Z"));
        }
    }
    state
}

fn check(
    scenario: &str,
    state: &BoardState,
    width: u16,
    height: u16,
    tier: ColorTier,
    glyphs: GlyphSet,
) {
    let name = format!(
        "mock-unknowns-{scenario}-{width}x{height}-{}-{}",
        tier.as_str(),
        match glyphs {
            GlyphSet::Unicode => "unicode",
            GlyphSet::Ascii => "ascii",
        }
    );
    let context = FleetContext {
        now: Timestamp::new(NOW),
        load: LoadState::Ready,
    };
    let rendered = dump_frame(width, height, tier, glyphs, |area, buf, tier, glyphs| {
        let mut hits = HitMap::new();
        render_fleet(area, buf, state, &context, None, tier, glyphs, &mut hits);
    });
    assert_matches_golden(&name, &rendered);
}

#[test]
fn footer_at_120x40() {
    check(
        "footer",
        &partial(),
        120,
        40,
        ColorTier::Truecolor,
        GlyphSet::Unicode,
    );
}

/// The rung the non-tty snapshot actually renders at (three call sites
/// hardcode 100x30), so it is a real frame rather than an interpolation.
#[test]
fn footer_at_the_100x30_snapshot_rung() {
    check(
        "footer",
        &partial(),
        100,
        30,
        ColorTier::Truecolor,
        GlyphSet::Unicode,
    );
}

/// The floor, and the rung where the block gives up its worded rows.
#[test]
fn footer_at_the_80x24_floor() {
    check(
        "footer",
        &partial(),
        80,
        24,
        ColorTier::Truecolor,
        GlyphSet::Unicode,
    );
}

/// The degraded tier, which is where a treatment leaning on colour vanishes:
/// every token these rows use resolves to the terminal's own foreground at
/// Mono, so the words are the whole signal or there is none.
#[test]
fn footer_degraded_mono_ascii() {
    check(
        "footer",
        &partial(),
        120,
        40,
        ColorTier::Mono,
        GlyphSet::Ascii,
    );
}

/// Floor and degraded tier together — the worst frame the console can be asked
/// for, and the one a treatment is most likely to collapse in.
#[test]
fn footer_at_the_floor_degraded_mono_ascii() {
    check(
        "footer",
        &partial(),
        80,
        24,
        ColorTier::Mono,
        GlyphSet::Ascii,
    );
}

#[test]
fn seeded_workday_states_the_one_fact_it_is_missing() {
    check(
        "seeded",
        &seeded(),
        120,
        40,
        ColorTier::Truecolor,
        GlyphSet::Unicode,
    );
}

#[test]
fn a_page_with_nothing_unknown_carries_no_block() {
    check(
        "clean",
        &clean(),
        120,
        40,
        ColorTier::Truecolor,
        GlyphSet::Unicode,
    );
}
