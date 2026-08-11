//! The Rung-2 mockup pipeline: `ratatui` `TestBackend` renders of every TUI
//! lens over the seeded workday estate (`tests/common/estate.rs`), committed
//! as plain-text goldens — the substrate for the design-mockup loop. See
//! `tests/common/README.md`.
//!
//! One `#[test]` per golden, deliberately: `BLESS=1` panics after writing a
//! golden (`common::assert_matches_golden`'s faithfully-copied bless-run
//! failure), so a loop of several `Variant::check` calls in one `#[test]`
//! would only ever bless its first iteration before the panic unwound the
//! rest away.

mod common;

use common::Variant;
use common::estate::{
    drilldown_attached, empty_frame_input, estate_board_state, estate_config_state,
    estate_frame_input, estate_queue_state,
};
use gwk_theme::marks::GlyphSet;
use gwk_theme::tier::ColorTier;
use gwk_tui::board::{self, BoardState, BoardView};
use gwk_tui::config;
use gwk_tui::drilldown;
use gwk_tui::hall;
use gwk_tui::input::HitMap;
use gwk_tui::queue;

fn board_check(scenario: &'static str, width: u16, height: u16, state: &BoardState) {
    Variant::new("board", scenario, width, height).check(|area, buf, tier, glyphs| {
        let mut hits = HitMap::new();
        board::render(area, buf, state, None, tier, glyphs, &mut hits);
    });
}

// The design session's baseline: all ten Board panels — the nine views with
// no CLI equivalent included — over the same estate at 120x40.

#[test]
fn board_estate() {
    board_check("estate", 120, 40, &estate_board_state(BoardView::Estate));
}

#[test]
fn board_activity() {
    board_check("brief", 120, 40, &estate_board_state(BoardView::Activity));
}

#[test]
fn board_runs() {
    board_check("run", 120, 40, &estate_board_state(BoardView::Runs));
}

#[test]
fn board_dag() {
    board_check("dag", 120, 40, &estate_board_state(BoardView::Dag));
}

#[test]
fn board_flow() {
    board_check("flow", 120, 40, &estate_board_state(BoardView::Flow));
}

#[test]
fn board_events() {
    board_check("events", 120, 40, &estate_board_state(BoardView::Events));
}

#[test]
fn board_replay() {
    board_check("replay", 120, 40, &estate_board_state(BoardView::Replay));
}

#[test]
fn board_fleet() {
    board_check("fleet", 120, 40, &estate_board_state(BoardView::Fleet));
}

#[test]
fn board_cost_health() {
    board_check("cost", 120, 40, &estate_board_state(BoardView::CostHealth));
}

#[test]
fn board_audit() {
    board_check("audit", 120, 40, &estate_board_state(BoardView::Audit));
}

// The density floor: Fleet and Cost/Health are the two panels with the most
// columns, checked again at a small terminal size.

#[test]
fn board_fleet_at_80x24() {
    board_check("fleet", 80, 24, &estate_board_state(BoardView::Fleet));
}

#[test]
fn board_cost_health_at_80x24() {
    board_check("cost", 80, 24, &estate_board_state(BoardView::CostHealth));
}

#[test]
fn hall_estate_at_120x40_truecolor_unicode() {
    let input = estate_frame_input();
    Variant::new("hall", "estate", 120, 40).check(|area, buf, tier, glyphs| {
        let mut hits = HitMap::new();
        hall::render(area, buf, &input, tier, glyphs, &mut hits);
    });
}

#[test]
fn hall_estate_at_80x24() {
    let input = estate_frame_input();
    Variant::new("hall", "estate", 80, 24).check(|area, buf, tier, glyphs| {
        let mut hits = HitMap::new();
        hall::render(area, buf, &input, tier, glyphs, &mut hits);
    });
}

#[test]
fn hall_estate_ascii_mono() {
    let input = estate_frame_input();
    Variant::new("hall", "estate", 120, 40)
        .with_tier(ColorTier::Mono)
        .with_glyphs(GlyphSet::Ascii)
        .check(|area, buf, tier, glyphs| {
            let mut hits = HitMap::new();
            hall::render(area, buf, &input, tier, glyphs, &mut hits);
        });
}

#[test]
fn hall_estate_ansi16_unicode() {
    let input = estate_frame_input();
    Variant::new("hall", "estate", 120, 40)
        .with_tier(ColorTier::Ansi16)
        .check(|area, buf, tier, glyphs| {
            let mut hits = HitMap::new();
            hall::render(area, buf, &input, tier, glyphs, &mut hits);
        });
}

#[test]
fn hall_empty() {
    let input = empty_frame_input();
    Variant::new("hall", "empty", 80, 24).check(|area, buf, tier, glyphs| {
        let mut hits = HitMap::new();
        hall::render(area, buf, &input, tier, glyphs, &mut hits);
    });
}

#[test]
fn queue_estate_default() {
    let state = estate_queue_state();
    Variant::new("queue", "estate", 120, 40).check(|area, buf, tier, glyphs| {
        let mut hits = HitMap::new();
        queue::render(area, buf, &state, None, tier, glyphs, &mut hits);
    });
}

#[test]
fn queue_estate_ascii_mono() {
    let state = estate_queue_state();
    Variant::new("queue", "estate", 120, 40)
        .with_tier(ColorTier::Mono)
        .with_glyphs(GlyphSet::Ascii)
        .check(|area, buf, tier, glyphs| {
            let mut hits = HitMap::new();
            queue::render(area, buf, &state, None, tier, glyphs, &mut hits);
        });
}

#[test]
fn config_estate_default() {
    let state = estate_config_state();
    Variant::new("config", "estate", 120, 40).check(|area, buf, tier, glyphs| {
        let mut hits = HitMap::new();
        config::render(area, buf, &state, None, tier, glyphs, &mut hits);
    });
}

#[test]
fn drilldown_attached_default() {
    let state = drilldown_attached();
    Variant::new("drilldown", "attached", 100, 31).check(|area, buf, tier, _glyphs| {
        let mut hits = HitMap::new();
        drilldown::render(area, buf, &state, None, tier, &mut hits);
    });
}
