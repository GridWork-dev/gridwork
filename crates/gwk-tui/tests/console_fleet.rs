mod common;

use common::dump_frame;
use gwk_domain::ids::Timestamp;
use gwk_theme::marks::GlyphSet;
use gwk_theme::tier::ColorTier;
use gwk_tui::board::{BoardTarget, BoardView};
use gwk_tui::console::{FleetContext, LoadState, render_fleet};
use gwk_tui::input::HitMap;

fn render(width: u16, height: u16, tier: ColorTier, glyphs: GlyphSet) -> String {
    render_at(width, height, tier, glyphs, "2026-08-11T17:30:00Z")
}

fn render_at(width: u16, height: u16, tier: ColorTier, glyphs: GlyphSet, now: &str) -> String {
    let state = common::estate::estate_board_state(BoardView::Fleet);
    let context = FleetContext {
        now: Timestamp::new(now),
        load: LoadState::Ready,
    };
    dump_frame(width, height, tier, glyphs, |area, buf, tier, glyphs| {
        let mut hits = HitMap::<BoardTarget>::new();
        render_fleet(
            area,
            buf,
            &state,
            &context,
            state
                .attempts
                .get(1)
                .map(|attempt| BoardTarget::Attempt(attempt.id.clone()))
                .as_ref(),
            tier,
            glyphs,
            &mut hits,
        );
    })
}

#[test]
fn fleet_joins_cost_sessions_leases_and_unclaimed_resources() {
    let rendered = render(120, 40, ColorTier::Truecolor, GlyphSet::Unicode);
    assert!(rendered.contains("at-tui-impl"), "{rendered}");
    assert!(
        rendered.contains("$2.10"),
        "joined session cost is absent:\n{rendered}"
    );
    assert!(rendered.contains("UNCLAIMED"), "{rendered}");
    assert!(rendered.contains("ls-release"), "{rendered}");
    assert!(rendered.contains("wt-release"), "{rendered}");
    assert!(rendered.contains("SPEND / HOUR"), "{rendered}");
}

#[test]
fn fleet_floor_drops_whole_low_priority_columns() {
    let rendered = render(80, 24, ColorTier::Truecolor, GlyphSet::Unicode);
    let header = rendered.lines().nth(2).unwrap_or_default();
    assert!(header.contains("ATTEMPT"), "{header}");
    assert!(header.contains("STATE"), "{header}");
    assert!(header.contains("SPEND"), "{header}");
    assert!(!header.contains("TOKENS"), "{header}");
    assert!(rendered.contains("> at-pty-impl"), "{rendered}");
}

#[test]
fn fleet_degrades_marks_without_losing_state_words() {
    let rendered = render(120, 40, ColorTier::Mono, GlyphSet::Ascii);
    assert!(rendered.contains("- running"), "{rendered}");
    assert!(rendered.contains("X failed"), "{rendered}");
}

#[test]
fn fleet_age_cell_never_cuts_mid_value_at_the_design_width() {
    // Twenty-one hours after the seeded 08:45 attempt the AGE value is five
    // cells ("21h00"), which the last column only has once it budgets to
    // the row edge — the phantom gutter budgeted four and painted "21h0",
    // a plausible smaller age.
    let rendered = render_at(
        120,
        40,
        ColorTier::Truecolor,
        GlyphSet::Unicode,
        "2026-08-12T05:45:00Z",
    );
    assert!(rendered.contains("21h00"), "{rendered}");

    // Twelve days on, the six-cell "11d20h" cannot fit at all: it drops
    // whole behind the ruled '+' omission mark instead of painting the
    // five-cell lie "11d20" (or the four-cell "11d2").
    let rendered = render_at(
        120,
        40,
        ColorTier::Truecolor,
        GlyphSet::Unicode,
        "2026-08-23T05:45:00Z",
    );
    assert!(!rendered.contains("11d2"), "{rendered}");
}
