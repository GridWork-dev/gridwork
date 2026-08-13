mod common;

use gwk_domain::ids::{CostMicros, Seq, Timestamp};
use gwk_theme::marks::GlyphSet;
use gwk_theme::tier::ColorTier;
use gwk_tui::board::{self, BoardView};
use gwk_tui::console::dollars;
use gwk_tui::input::HitMap;
use gwk_tui::tables::{PageMeta, attempt_table, cost_table, session_table, term_table};

fn golden_body(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("goldens")
        .join(format!("{name}.txt"));
    std::fs::read_to_string(path)
        .expect("read mock CLI golden")
        .lines()
        .skip(1)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

fn complete() -> PageMeta {
    PageMeta {
        complete: true,
        watermark: Some(Seq::new(221)),
        next_cursor: None,
    }
}

#[test]
fn attempt_table_is_the_fleet_lens_column_contract() {
    let state = common::estate::estate_board_state(BoardView::Fleet);
    assert_eq!(
        attempt_table(
            &state,
            &Timestamp::new("2026-08-11T17:30:00Z"),
            &complete(),
            120,
        ),
        golden_body("mock-cli-attempt-list-120col")
    );
}

#[test]
fn term_and_session_tables_keep_the_two_session_nouns_distinct() {
    let (running, closed) = common::estate::estate_pty_sessions();
    assert_eq!(
        term_table(&[running, closed], &complete(), 120),
        golden_body("mock-cli-term-list-120col")
    );

    let state = common::estate::estate_board_state(BoardView::Fleet);
    let partial = PageMeta {
        complete: false,
        watermark: Some(Seq::new(221)),
        next_cursor: Some("c2VxOjIyMQ".to_owned()),
    };
    assert_eq!(
        session_table(&state.sessions, &partial, 120),
        golden_body("mock-cli-session-list-120col")
    );
}

#[test]
fn cost_table_adds_the_ruled_time_axis_without_repricing_unknowns() {
    let state = common::estate::estate_board_state(BoardView::CostHealth);
    assert_eq!(
        cost_table(
            &state,
            &Timestamp::new("2026-08-11T17:30:00Z"),
            &complete(),
            120,
        ),
        golden_body("mock-cli-cost-rollup-120col")
    );
}

#[test]
fn terminal_tables_escape_control_text_before_printing_it() {
    let (mut session, _) = common::estate::estate_pty_sessions();
    session.title = Some("\u{1b}[2Jspoof\nSTATE".to_owned());

    let rendered = term_table(&[session], &complete(), 120);
    assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
    assert!(
        rendered.contains("\\u{1B}[2Jspoof\\u{A}STATE"),
        "{rendered}"
    );
}

#[test]
fn zero_and_maximum_costs_render_without_panicking_or_overflowing() {
    let mut state = common::estate::estate_board_state(BoardView::CostHealth);
    let mut entry = state.costs[0].clone();
    entry.cost_micros = Some(CostMicros::new(0));
    state.costs = vec![entry];

    let rendered = cost_table(
        &state,
        &Timestamp::new("2026-08-11T17:30:00Z"),
        &complete(),
        120,
    );
    assert!(rendered.contains("$0.00"), "{rendered}");
    assert_eq!(dollars(u64::MAX), "$18446744073709.55");
}

#[test]
fn cost_table_scopes_today_and_marks_unreported_tokens() {
    let mut state = common::estate::estate_board_state(BoardView::CostHealth);
    let mut entry = state.costs[0].clone();
    entry.recorded_at = Timestamp::new("2026-08-10T09:00:00Z");
    entry.input_tokens = None;
    entry.output_tokens = None;
    let mut current = state.costs[1].clone();
    current.input_tokens = None;
    current.output_tokens = None;
    state.costs = vec![entry, current];

    let rendered = cost_table(
        &state,
        &Timestamp::new("2026-08-11T17:30:00Z"),
        &complete(),
        120,
    );
    assert!(rendered.contains("0+?/0+?"), "{rendered}");
    assert!(!rendered.contains("09:00"), "{rendered}");
}

#[test]
fn cost_table_says_the_ledger_is_empty_rather_than_printing_a_zero_total() {
    // The same command with stdout not a terminal emits `cost_rollup`, which
    // answers `cost_micros: null` and pins the reason under `unknowns`. This is
    // the human half of ONE command, so it cannot report a measured zero where
    // the machine half reports an absence -- that drift tells two readers two
    // different things about the same page.
    let mut state = common::estate::estate_board_state(BoardView::CostHealth);
    state.costs.clear();
    let rendered = cost_table(
        &state,
        &Timestamp::new("2026-08-11T17:30:00Z"),
        &complete(),
        120,
    );
    assert!(
        !rendered.contains("$0.00"),
        "an empty ledger priced itself:\n{rendered}"
    );
    assert!(
        rendered.contains("no entries -- no spend recorded on this page"),
        "the JSON twin own sentence is missing:\n{rendered}"
    );
    // And no header-only tables, which are what made the zero look surveyed.
    assert!(!rendered.contains("BY LANE"), "{rendered}");
    assert!(!rendered.contains("BY HOUR"), "{rendered}");
    assert!(rendered.contains("watermark 221"), "{rendered}");
}

#[test]
fn both_ingestion_kinds_explain_a_zero_count_not_just_the_health_one() {
    // `health` and `session` share one absent producer, and only `health`
    // carried a note. The asymmetry is worse than a plain omission: a reader
    // who has just been told why `health` reads zero takes the unexplained
    // count beside it for a measurement.
    //
    // Checked at mono, deliberately. The rows are marked with the `unknown`
    // binding, which resolves to a token that degrades to the terminal's own
    // foreground there — so colour alone leaves the row indistinguishable
    // from a counted one, and only words survive every tier.
    let mut state = common::estate::estate_board_state(BoardView::CostHealth);
    state.ingested.clear();
    for (tier, glyphs) in [
        (ColorTier::Truecolor, GlyphSet::Unicode),
        (ColorTier::Mono, GlyphSet::Ascii),
    ] {
        let rendered = common::dump_frame(120, 40, tier, glyphs, |area, buf, tier, glyphs| {
            let mut hits = HitMap::new();
            board::render(area, buf, &state, None, tier, glyphs, &mut hits);
        });
        assert_eq!(
            rendered
                .matches("ingestion is operator-driven, no producer")
                .count(),
            2,
            "one ingestion kind explains its zero and the other does not, at {}:\n{rendered}",
            tier.as_str()
        );
        assert!(
            rendered.contains("session: no records"),
            "the session half has no note at {}:\n{rendered}",
            tier.as_str()
        );
    }
}

#[test]
fn cost_table_keeps_its_tables_when_entries_land_without_a_price() {
    // Entries carrying no price are a different state from no entries at all:
    // the rows are real and stay, and only the total is withheld.
    let mut state = common::estate::estate_board_state(BoardView::CostHealth);
    for entry in &mut state.costs {
        entry.cost_micros = None;
    }
    let rendered = cost_table(
        &state,
        &Timestamp::new("2026-08-11T17:30:00Z"),
        &complete(),
        120,
    );
    assert!(rendered.contains("BY LANE"), "{rendered}");
    assert!(
        rendered.contains("no cost reported"),
        "an unpriced page still printed a total:\n{rendered}"
    );
    assert!(!rendered.contains("$0.00"), "{rendered}");
    assert!(rendered.contains("0 priced"), "{rendered}");
}
