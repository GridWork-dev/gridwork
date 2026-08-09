//! Render every Board view from real kernel pages — the task's smoke run.
//!
//! ```text
//! for k in task attempt dispatch_node message engine_session worktree \
//!          lease cost_entry ingested_record receipt; do
//!   gw projection list $k --limit 256
//! done | jq -s . | cargo run -p gwk-tui --example board-live
//! ```
//!
//! Standard input is a JSON array of `projection_page` answers (each
//! `{"records": [...], ...}`); a bare array of tagged records also works.
//! Every record decodes through the contract's `ProjectionRecord` — this is
//! wire data walking the same door the console will use, not a fixture.
//!
//! The smoke run reads a bounded prefix and says so: it never claims the
//! read reached the end of a projection, so `BoardState::complete` stays
//! `false` and every folded figure the cost panel prints is a floor. A
//! caller that genuinely paged to exhaustion is the one entitled to set it.

use std::io::Read;
use std::process::ExitCode;

use gwk_domain::ids::Seq;
use gwk_domain::protocol::ProjectionRecord;
use gwk_theme::marks::GlyphSet;
use gwk_theme::tier::ColorTier;
use gwk_tui::board::{BoardState, BoardView, render};
use gwk_tui::input::HitMap;
use gwk_tui::replay::ReplayTimeline;
use gwk_tui::theme::safe_text;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_DIAGNOSTIC_CELLS: usize = 512;
const MAX_RECORDS: usize = 10 * 256;

fn diagnostic(why: &str) -> String {
    safe_text(why, MAX_DIAGNOSTIC_CELLS).into_owned()
}

fn read_input(reader: impl Read) -> Result<serde_json::Value, String> {
    let mut bytes = Vec::new();
    reader
        .take((MAX_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|why| format!("stdin could not be read: {why}"))?;
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(format!(
            "input exceeds the {MAX_INPUT_BYTES}-byte smoke-run limit"
        ));
    }
    serde_json::from_slice(&bytes).map_err(|why| format!("stdin is not JSON: {why}"))
}

fn empty_state() -> BoardState {
    BoardState {
        view: BoardView::Dag,
        tasks: Vec::new(),
        attempts: Vec::new(),
        nodes: Vec::new(),
        messages: Vec::new(),
        replay: ReplayTimeline::empty(),
        sessions: Vec::new(),
        worktrees: Vec::new(),
        leases: Vec::new(),
        costs: Vec::new(),
        ingested: Vec::new(),
        receipts: Vec::new(),
        // A bounded smoke run over whatever the caller piped in is a prefix
        // by construction. Claiming otherwise would turn the cost panel's
        // floors into totals it has no standing to assert.
        complete: false,
        watermark: None,
    }
}

fn add_record(
    state: &mut BoardState,
    value: serde_json::Value,
    record_count: &mut usize,
) -> Result<(), String> {
    *record_count += 1;
    if *record_count > MAX_RECORDS {
        return Err(format!(
            "input exceeds the {MAX_RECORDS}-record smoke-run limit"
        ));
    }
    match serde_json::from_value::<ProjectionRecord>(value)
        .map_err(|why| format!("a record failed the contract: {why}"))?
    {
        ProjectionRecord::Task { task } => state.tasks.push(task),
        ProjectionRecord::Attempt { attempt } => state.attempts.push(attempt),
        ProjectionRecord::DispatchNode { dispatch_node } => state.nodes.push(dispatch_node),
        ProjectionRecord::Message { message } => state.messages.push(message),
        ProjectionRecord::EngineSession { engine_session } => state.sessions.push(engine_session),
        ProjectionRecord::Worktree { worktree } => state.worktrees.push(worktree),
        ProjectionRecord::Lease { lease } => state.leases.push(lease),
        ProjectionRecord::CostEntry { cost_entry } => state.costs.push(cost_entry),
        ProjectionRecord::IngestedRecord { ingested_record } => {
            state.ingested.push(ingested_record);
        }
        ProjectionRecord::Receipt { receipt } => state.receipts.push(receipt),
        _ => {}
    }
    Ok(())
}

fn load_state(input: serde_json::Value) -> Result<BoardState, String> {
    let mut state = empty_state();
    let mut record_count = 0;
    let items = match input {
        serde_json::Value::Array(items) => items,
        one => vec![one],
    };
    let bare_records = items
        .iter()
        .all(|item| item.get("projection_type").is_some());

    if bare_records {
        for record in items {
            add_record(&mut state, record, &mut record_count)?;
        }
        return Ok(state);
    }

    for page in items {
        if let Some(mark) = page.get("watermark") {
            let seq = serde_json::from_value::<Seq>(mark.clone())
                .map_err(|why| format!("a page watermark failed the contract: {why}"))?;
            state.watermark = Some(state.watermark.map_or(seq, |current| current.min(seq)));
        }
        let records = page
            .get("records")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| {
                "input item is neither a projection page nor a tagged record".to_string()
            })?;
        for value in records {
            add_record(&mut state, value.clone(), &mut record_count)?;
        }
    }
    Ok(state)
}

fn main() -> ExitCode {
    let input = match read_input(std::io::stdin().lock()) {
        Ok(input) => input,
        Err(why) => {
            eprintln!("board-live: {}", diagnostic(&why));
            return ExitCode::FAILURE;
        }
    };
    let mut state = match load_state(input) {
        Ok(state) => state,
        Err(why) => {
            eprintln!("board-live: {}", diagnostic(&why));
            return ExitCode::FAILURE;
        }
    };

    let area = Rect::new(0, 0, 100, 30);
    for view in BoardView::ALL {
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
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use gwk_domain::entity::Task;
    use gwk_domain::fsm::TaskState;
    use gwk_domain::ids::{TaskId, Timestamp};

    use super::*;

    fn task_record(id: &str) -> serde_json::Value {
        serde_json::to_value(ProjectionRecord::Task {
            task: Task {
                id: TaskId::new(id),
                version: 1,
                state: TaskState::Working,
                kind: None,
                title: Some("smoke task".into()),
                spec_ref: None,
                project: None,
                priority: None,
                tracker_ref: None,
                created_at: Timestamp::new("2026-08-06T10:00:00Z"),
                updated_at: Timestamp::new("2026-08-06T10:00:00Z"),
            },
        })
        .expect("record serializes")
    }

    #[test]
    fn bare_record_arrays_walk_the_contract() {
        let state = load_state(serde_json::json!([task_record("t-1")])).expect("bare records");
        assert_eq!(state.tasks.len(), 1);
        assert_eq!(state.tasks[0].id.as_str(), "t-1");
    }

    #[test]
    fn combined_pages_use_the_oldest_watermark() {
        let state = load_state(serde_json::json!([
            {"records": [task_record("t-1")], "watermark": "12"},
            {"records": [], "watermark": "9"}
        ]))
        .expect("pages");
        assert_eq!(state.watermark, Some(Seq::new(9)));
    }

    #[test]
    fn malformed_input_cannot_print_an_empty_success_frame() {
        let error = load_state(serde_json::json!({"not_records": []})).expect_err("invalid input");
        assert!(error.contains("neither a projection page nor a tagged record"));
    }

    #[test]
    fn diagnostics_escape_terminal_controls() {
        let rendered = diagnostic("bad \u{1b}]0;owned\u{7}");
        assert!(!rendered.contains('\u{1b}'));
        assert!(!rendered.contains('\u{7}'));
        assert_eq!(rendered, "bad \\u{1B}]0;owned\\u{7}");
    }

    #[test]
    fn stdin_is_bounded_before_json_allocation() {
        let bytes = vec![b' '; MAX_INPUT_BYTES + 1];
        let error = read_input(Cursor::new(bytes)).expect_err("oversized input");
        assert!(error.contains("input exceeds"));
    }

    #[test]
    fn projection_records_are_bounded_to_the_documented_ten_pages() {
        let records = vec![task_record("t-1"); MAX_RECORDS + 1];
        let error = load_state(serde_json::Value::Array(records)).expect_err("too many records");
        assert!(error.contains("record smoke-run limit"));
    }
}
