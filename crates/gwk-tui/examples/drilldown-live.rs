//! Paint a hosted PTY session from typed wire controls.
//!
//! Standard input accepts one `ServerControl`, an array of controls, or a
//! newline-delimited wire capture. The first PTY control selects the session;
//! every value then passes through the contract and the drill-down mirror.

use std::io::{IsTerminal, Read};
use std::process::ExitCode;

use gwk_domain::ids::PtySessionId;
use gwk_domain::protocol::KernelResult;
use gwk_domain::protocol::ServerControl;
use gwk_theme::tier::{ColorChoice, ColorTier, TerminalEnv};
use gwk_tui::drilldown::{DrilldownState, IngestDisposition, render};
use gwk_tui::input::HitMap;
use gwk_tui::theme::safe_text;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::{Terminal, TerminalOptions, Viewport};

const MAX_INPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_DIAGNOSTIC_CELLS: usize = 512;

fn diagnostic(why: &str) -> String {
    safe_text(why, MAX_DIAGNOSTIC_CELLS).into_owned()
}

fn decode_controls(reader: impl Read) -> Result<Vec<ServerControl>, String> {
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
    if let Ok(controls) = serde_json::from_slice::<Vec<ServerControl>>(&bytes) {
        if controls.is_empty() {
            return Err("input contains no wire controls".to_owned());
        }
        return Ok(controls);
    }
    if let Ok(control) = serde_json::from_slice::<ServerControl>(&bytes) {
        return Ok(vec![control]);
    }
    let controls = serde_json::Deserializer::from_slice(&bytes)
        .into_iter::<ServerControl>()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|why| format!("stdin is not typed server-control JSON: {why}"))?;
    if controls.is_empty() {
        return Err("input contains no wire controls".to_owned());
    }
    Ok(controls)
}

fn session_id(control: &ServerControl) -> Option<&PtySessionId> {
    match control {
        ServerControl::Response {
            result: KernelResult::PtyAttached { session_id, .. },
            ..
        }
        | ServerControl::Response {
            result: KernelResult::PtySnapshot { session_id, .. },
            ..
        }
        | ServerControl::PtyDeltaBatch { session_id, .. } => Some(session_id),
        _ => None,
    }
}

fn main() -> ExitCode {
    let controls = match decode_controls(std::io::stdin().lock()) {
        Ok(controls) => controls,
        Err(why) => {
            eprintln!("drilldown-live: {}", diagnostic(&why));
            return ExitCode::FAILURE;
        }
    };
    let Some(session_id) = controls.iter().find_map(session_id).cloned() else {
        eprintln!("drilldown-live: input contains no PTY session control");
        return ExitCode::FAILURE;
    };
    let mut state = DrilldownState::new(session_id);
    let mut consumed = 0usize;
    for control in &controls {
        if state.ingest(control) != IngestDisposition::Unrelated {
            consumed += 1;
        }
    }
    if consumed == 0 {
        eprintln!("drilldown-live: input contains no controls for the selected session");
        return ExitCode::FAILURE;
    }

    let tier = ColorTier::resolve(&TerminalEnv::from_process(ColorChoice::Auto));
    let stdout = std::io::stdout();
    let viewport = if stdout.is_terminal() {
        Viewport::Fullscreen
    } else {
        // A captured smoke run still emits a complete styled frame instead of
        // failing terminal-size discovery on a pipe.
        Viewport::Fixed(Rect::new(0, 0, 100, 30))
    };
    let mut terminal = match Terminal::with_options(
        CrosstermBackend::new(stdout.lock()),
        TerminalOptions { viewport },
    ) {
        Ok(terminal) => terminal,
        Err(why) => {
            eprintln!("drilldown-live: terminal could not be opened: {why}");
            return ExitCode::FAILURE;
        }
    };
    let mut hits = HitMap::new();
    if let Err(why) = terminal.draw(|frame| {
        render(
            frame.area(),
            frame.buffer_mut(),
            &state,
            None,
            tier,
            &mut hits,
        );
    }) {
        eprintln!("drilldown-live: frame could not be painted: {why}");
        return ExitCode::FAILURE;
    }
    let _ = terminal.show_cursor();
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use gwk_domain::ids::{PtyFrameSeq, PtySessionGeneration, PtySessionId, RequestId};
    use gwk_domain::protocol::{KernelResult, ServerControl};

    use super::*;

    fn snapshot() -> ServerControl {
        ServerControl::Response {
            request_id: RequestId::new("snapshot-1"),
            result: KernelResult::PtySnapshot {
                session_id: PtySessionId::new("pty-1"),
                generation: PtySessionGeneration::new("pty-life-1"),
                seq: PtyFrameSeq::new(1),
                frame: gwk_domain::frame::PtyFrame {
                    styles: Vec::new(),
                    rows: Vec::new(),
                },
            },
        }
    }

    #[test]
    fn drilldown_live_accepts_array_single_and_newline_wire_controls() {
        let one = serde_json::to_vec(&snapshot()).expect("serialize control");
        assert_eq!(
            decode_controls(Cursor::new(&one)).expect("one"),
            vec![snapshot()]
        );

        let array = serde_json::to_vec(&vec![snapshot(), snapshot()]).expect("serialize array");
        assert_eq!(decode_controls(Cursor::new(array)).expect("array").len(), 2);

        let mut lines = one.clone();
        lines.push(b'\n');
        lines.extend_from_slice(&one);
        assert_eq!(decode_controls(Cursor::new(lines)).expect("lines").len(), 2);
    }

    #[test]
    fn drilldown_live_bounds_input_before_json_allocation() {
        let bytes = vec![b' '; MAX_INPUT_BYTES + 1];
        let error = decode_controls(Cursor::new(bytes)).expect_err("oversized input");
        assert!(error.contains("input exceeds"), "{error}");
    }
}
