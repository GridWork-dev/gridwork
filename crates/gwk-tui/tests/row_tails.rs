mod common;

use common::dump_frame;
use gwk_theme::marks::GlyphSet;
use gwk_theme::tier::ColorTier;
use gwk_tui::board::{self, BoardTarget, BoardView};
use gwk_tui::config::{self, ConfigTarget};
use gwk_tui::input::HitMap;
use gwk_tui::queue::{self, QueueTarget};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

#[test]
fn narrow_rows_mark_an_omitted_tail_instead_of_dropping_it_silently() {
    let queue_state = common::estate::estate_queue_state();
    let queue = dump_frame(
        25,
        10,
        ColorTier::Mono,
        GlyphSet::Ascii,
        |area, buf, tier, glyphs| {
            let mut hits = HitMap::<QueueTarget>::new();
            queue::render(area, buf, &queue_state, None, tier, glyphs, &mut hits);
        },
    );
    assert!(
        queue.lines().next().unwrap_or_default().ends_with('+'),
        "{queue}"
    );

    let board_state = common::estate::estate_board_state(BoardView::Runs);
    let board = dump_frame(
        25,
        10,
        ColorTier::Mono,
        GlyphSet::Ascii,
        |area, buf, tier, glyphs| {
            let mut hits = HitMap::<BoardTarget>::new();
            board::render(area, buf, &board_state, None, tier, glyphs, &mut hits);
        },
    );
    assert!(board.lines().any(|line| line.ends_with('+')), "{board}");

    let config_state = common::estate::estate_config_state();
    let config = dump_frame(
        40,
        10,
        ColorTier::Mono,
        GlyphSet::Ascii,
        |area, buf, tier, glyphs| {
            let mut hits = HitMap::<ConfigTarget>::new();
            config::render(area, buf, &config_state, None, tier, glyphs, &mut hits);
        },
    );
    assert!(config.lines().any(|line| line.ends_with('+')), "{config}");
}

#[test]
fn deep_graph_rows_cap_indent_and_state_the_real_depth() {
    let mut state = common::estate::estate_board_state(BoardView::Dag);
    let template = state.nodes[0].clone();
    state.nodes.clear();
    let mut parent = None;
    for index in 0..7 {
        let id = gwk_domain::ids::DispatchNodeId::new(format!("d-deep-{index}"));
        state.nodes.push(gwk_domain::entity::DispatchNode {
            id: id.clone(),
            parent_id: parent,
            attempt_id: Some(gwk_domain::ids::AttemptId::new("at-pty-impl")),
            label: Some(format!("depth-{index}")),
            ..template.clone()
        });
        parent = Some(id);
    }

    let rendered = dump_frame(
        120,
        30,
        ColorTier::Mono,
        GlyphSet::Ascii,
        |area, buf, tier, glyphs| {
            let mut hits = HitMap::<BoardTarget>::new();
            board::render(area, buf, &state, None, tier, glyphs, &mut hits);
        },
    );
    assert!(rendered.contains("+7"), "{rendered}");
    assert!(rendered.contains("+8"), "{rendered}");
}

#[test]
fn tail_paints_only_a_complete_escape_and_ignores_empty_geometry() {
    let area = Rect::new(0, 0, 10, 1);
    let mut buffer = Buffer::empty(area);
    gwk_tui::row::paint_tail(&mut buffer, area, 0, 1, "ok🙂", Style::default());
    assert_eq!(buffer[(9, 0)].symbol(), "+");

    let empty = Rect::new(0, 0, 1, 0);
    gwk_tui::row::paint_tail(&mut buffer, empty, 0, 0, "tail", Style::default());
}
