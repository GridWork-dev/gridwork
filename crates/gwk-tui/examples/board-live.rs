//! Render both Board views from real kernel pages — the task's smoke run.
//!
//! ```text
//! for k in task attempt dispatch_node message; do
//!   gw projection list $k --limit 256
//! done | jq -s . | cargo run -p gwk-tui --example board-live
//! ```
//!
//! Standard input is a JSON array of `projection_page` answers (each
//! `{"records": [...], ...}`); a bare array of tagged records also works.
//! Every record decodes through the contract's `ProjectionRecord` — this is
//! wire data walking the same door the console will use, not a fixture.

use gwk_domain::ids::Seq;
use gwk_domain::protocol::ProjectionRecord;
use gwk_tui::board::{BoardState, BoardView, render};
use gwk_tui::input::HitMap;
use gwk_theme::marks::GlyphSet;
use gwk_theme::tier::ColorTier;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

fn main() {
    let input: serde_json::Value =
        serde_json::from_reader(std::io::stdin()).expect("stdin is JSON");
    let pages: Vec<serde_json::Value> = match input {
        serde_json::Value::Array(items) => items,
        one => vec![one],
    };

    let mut state = BoardState {
        view: BoardView::Dag,
        tasks: Vec::new(),
        attempts: Vec::new(),
        nodes: Vec::new(),
        messages: Vec::new(),
        watermark: None,
    };
    for page in pages {
        if let Some(mark) = page.get("watermark") {
            if let Ok(seq) = serde_json::from_value::<Seq>(mark.clone()) {
                state.watermark = Some(seq);
            }
        }
        let records = match page.get("records") {
            Some(serde_json::Value::Array(records)) => records.clone(),
            _ => match page {
                serde_json::Value::Array(records) => records,
                _ => Vec::new(),
            },
        };
        for value in records {
            match serde_json::from_value::<ProjectionRecord>(value) {
                Ok(ProjectionRecord::Task { task }) => state.tasks.push(task),
                Ok(ProjectionRecord::Attempt { attempt }) => state.attempts.push(attempt),
                Ok(ProjectionRecord::DispatchNode { dispatch_node }) => {
                    state.nodes.push(dispatch_node)
                }
                Ok(ProjectionRecord::Message { message }) => state.messages.push(message),
                Ok(_) => {}
                Err(why) => panic!("a record failed the contract: {why}"),
            }
        }
    }

    let area = Rect::new(0, 0, 100, 30);
    for view in [BoardView::Dag, BoardView::Flow] {
        state.view = view;
        let mut buf = Buffer::empty(area);
        let mut hits: HitMap<gwk_tui::board::BoardTarget> = HitMap::new();
        render(
            area,
            &mut buf,
            &state,
            None,
            ColorTier::Mono,
            GlyphSet::Unicode,
            &mut hits,
        );
        println!("==== {view:?} ====");
        for y in 0..area.height {
            let mut line = String::new();
            for x in 0..area.width {
                line.push_str(buf[(x, y)].symbol());
            }
            println!("{}", line.trim_end());
        }
    }
}
