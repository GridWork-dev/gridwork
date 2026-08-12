mod common;

use gwk_domain::ids::{CostMicros, Seq, Timestamp};
use gwk_tui::board::BoardView;
use gwk_tui::console::dollars;
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
