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

/// The empty-ledger frame, which is the live one: nothing writes `cost_entry`
/// yet, so every money figure on this lens folds over nothing. A fold cannot
/// tell "summed to zero" from "summed over nothing" once it has run, so the
/// check is that neither the header nor the chart is willing to print a
/// dollar amount when no entry was priced.
fn render_without_costs(width: u16, height: u16, tier: ColorTier, glyphs: GlyphSet) -> String {
    let mut state = common::estate::estate_board_state(BoardView::Fleet);
    state.costs.clear();
    let context = FleetContext {
        now: Timestamp::new("2026-08-11T17:30:00Z"),
        load: LoadState::Ready,
    };
    dump_frame(width, height, tier, glyphs, |area, buf, tier, glyphs| {
        let mut hits = HitMap::<BoardTarget>::new();
        render_fleet(area, buf, &state, &context, None, tier, glyphs, &mut hits);
    })
}

#[test]
fn fleet_states_an_empty_cost_ledger_rather_than_pricing_it_at_zero() {
    // Every rung the terminal offers, because a degraded tier is where an
    // honest rendering is most likely to collapse back into a bare number.
    for (width, height, tier, glyphs) in [
        (120u16, 40u16, ColorTier::Truecolor, GlyphSet::Unicode),
        (80, 24, ColorTier::Truecolor, GlyphSet::Unicode),
        (120, 40, ColorTier::Mono, GlyphSet::Ascii),
    ] {
        let rendered = render_without_costs(width, height, tier, glyphs);
        let at = format!("at {width}x{height} {}", tier.as_str());

        // The header. `$0.00 today` beside a live attempt count reads as "the
        // estate ran and cost nothing", which is a measurement nobody took.
        assert!(
            !rendered.contains("$0.00"),
            "a zero fold priced itself {at}:\n{rendered}"
        );
        assert!(
            rendered.contains("no spend recorded"),
            "the header does not say the ledger is empty {at}:\n{rendered}"
        );

        // The chart. `peak` floors to one micro on an empty ledger, so the
        // scale and its bottom rung are drawn over data that does not exist.
        assert!(
            !rendered.contains("> $0"),
            "the spend chart drew a scale over nothing {at}:\n{rendered}"
        );
        assert!(
            rendered.contains("no scale to draw"),
            "the chart is silent about why it is blank {at}:\n{rendered}"
        );

        // The row counts stay: zero rows is a fact, and it is the one figure
        // here that was actually measured.
        assert!(
            rendered.contains("0 priced"),
            "the honest row counts went missing {at}:\n{rendered}"
        );
    }
}

#[test]
fn fleet_keeps_pricing_a_ledger_that_has_entries() {
    // The guard above must not have bought its honesty by refusing to ever
    // print a total: the seeded ledger is priced, and still prices.
    let rendered = render(120, 40, ColorTier::Truecolor, GlyphSet::Unicode);
    assert!(rendered.contains("$"), "{rendered}");
    assert!(!rendered.contains("no spend recorded"), "{rendered}");
    assert!(!rendered.contains("no scale to draw"), "{rendered}");
}
