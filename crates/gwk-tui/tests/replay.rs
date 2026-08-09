use gwk_theme::marks::GlyphSet;
use gwk_theme::tier::ColorTier;
use gwk_tui::board::{BoardState, BoardTarget, BoardView, render, target_order};
use gwk_tui::input::HitMap;
use gwk_tui::replay::{ReplayError, ReplayFrame, ReplayTimeline};
use ratatui::Terminal;
use ratatui::backend::TestBackend;

const MAGIC: &[u8; 8] = b"GWKREC\0\x01";

fn recording(entries: impl IntoIterator<Item = (u64, u64, Frame)>) -> Vec<u8> {
    let mut bytes = MAGIC.to_vec();
    for (seq, elapsed_ms, frame) in entries {
        bytes.extend_from_slice(&seq.to_le_bytes());
        bytes.extend_from_slice(&elapsed_ms.to_le_bytes());
        match frame {
            Frame::Output(output) => {
                bytes.push(0);
                let length = u32::try_from(output.len()).expect("test output is representable");
                bytes.extend_from_slice(&length.to_le_bytes());
                bytes.extend_from_slice(&output);
            }
            Frame::Resize { cols, rows } => {
                bytes.push(1);
                bytes.extend_from_slice(&cols.to_le_bytes());
                bytes.extend_from_slice(&rows.to_le_bytes());
            }
        }
    }
    bytes
}

enum Frame {
    Output(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

fn replay_state(timeline: ReplayTimeline) -> BoardState {
    BoardState {
        view: BoardView::Replay,
        tasks: Vec::new(),
        attempts: Vec::new(),
        nodes: Vec::new(),
        messages: Vec::new(),
        replay: timeline,
        sessions: Vec::new(),
        worktrees: Vec::new(),
        leases: Vec::new(),
        costs: Vec::new(),
        ingested: Vec::new(),
        complete: true,
        watermark: None,
    }
}

fn render_replay(
    state: &BoardState,
    selected: Option<&BoardTarget>,
) -> (String, HitMap<BoardTarget>) {
    let mut terminal = Terminal::new(TestBackend::new(80, 12)).expect("terminal");
    let mut hits = HitMap::new();
    terminal
        .draw(|frame| {
            render(
                frame.area(),
                frame.buffer_mut(),
                state,
                selected,
                ColorTier::Mono,
                GlyphSet::Unicode,
                &mut hits,
            );
        })
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let mut rendered = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            rendered.push_str(buffer[(x, y)].symbol());
        }
        rendered.push('\n');
    }
    (rendered, hits)
}

#[test]
fn replay_decodes_output_and_resize_in_recorded_order() {
    let bytes = recording([
        (4, 0, Frame::Output(vec![0, 0xff, b'\n'])),
        (
            5,
            17,
            Frame::Resize {
                cols: 120,
                rows: 42,
            },
        ),
        (6, 18, Frame::Output(b"\x1b[31mred".to_vec())),
    ]);

    let timeline = ReplayTimeline::decode(&bytes).expect("representative recording decodes");

    assert_eq!(
        timeline.frames(),
        [
            ReplayFrame::Output {
                seq: 4,
                elapsed_ms: 0,
                bytes: vec![0, 0xff, b'\n'],
            },
            ReplayFrame::Resize {
                seq: 5,
                elapsed_ms: 17,
                cols: 120,
                rows: 42,
            },
            ReplayFrame::Output {
                seq: 6,
                elapsed_ms: 18,
                bytes: b"\x1b[31mred".to_vec(),
            },
        ]
    );
    assert_eq!(ReplayTimeline::decode(&bytes), Ok(timeline));
}

#[test]
fn replay_refuses_malformed_and_out_of_order_streams() {
    assert_eq!(
        ReplayTimeline::decode(b"not a recording"),
        Err(ReplayError::Magic)
    );
    assert_eq!(ReplayTimeline::decode(MAGIC), Ok(ReplayTimeline::empty()));

    let truncated = &recording([(0, 0, Frame::Resize { cols: 80, rows: 24 })])[..24];
    assert_eq!(
        ReplayTimeline::decode(truncated),
        Err(ReplayError::Truncated)
    );

    let mut unknown_tag = recording([(0, 0, Frame::Resize { cols: 80, rows: 24 })]);
    unknown_tag[24] = 99;
    assert_eq!(
        ReplayTimeline::decode(&unknown_tag),
        Err(ReplayError::UnknownTag(99))
    );

    let sequence_out_of_order =
        recording([(4, 0, Frame::Output(vec![])), (4, 1, Frame::Output(vec![]))]);
    assert_eq!(
        ReplayTimeline::decode(&sequence_out_of_order),
        Err(ReplayError::SequenceOutOfOrder {
            previous: 4,
            found: 4,
        })
    );

    let elapsed_out_of_order =
        recording([(4, 2, Frame::Output(vec![])), (5, 1, Frame::Output(vec![]))]);
    assert_eq!(
        ReplayTimeline::decode(&elapsed_out_of_order),
        Err(ReplayError::ElapsedOutOfOrder {
            previous: 2,
            found: 1,
        })
    );

    for (cols, rows) in [(0, 24), (80, 0), (0, 0)] {
        let invalid_geometry = recording([(0, 0, Frame::Resize { cols, rows })]);
        assert_eq!(
            ReplayTimeline::decode(&invalid_geometry),
            Err(ReplayError::InvalidGeometry { cols, rows })
        );
    }

    let mut impossible_length = recording([(0, 0, Frame::Output(vec![]))]);
    impossible_length[25..29].copy_from_slice(&10_u32.to_le_bytes());
    assert_eq!(
        ReplayTimeline::decode(&impossible_length),
        Err(ReplayError::ImpossibleLength {
            declared: 10,
            remaining: 0,
        })
    );
}

#[test]
fn replay_board_panel_presents_recorded_frames_and_selected_detail() {
    let bytes = recording([
        (4, 0, Frame::Output(b"hello\n".to_vec())),
        (
            5,
            17,
            Frame::Resize {
                cols: 120,
                rows: 42,
            },
        ),
        (6, 18, Frame::Output(b"done".to_vec())),
    ]);
    let state = replay_state(ReplayTimeline::decode(&bytes).expect("recording decodes"));
    let selected = BoardTarget::ReplayFrame(4);

    let (rendered, hits) = render_replay(&state, Some(&selected));

    for expected in [
        "3 replay frames  18ms span",
        "0ms  output  6 bytes  hello\\u{A}",
        "17ms  resize  120x42",
        "frame 4  elapsed 0ms",
        "output 6 bytes",
        "BOARD  dag flow [replay]",
    ] {
        assert!(
            rendered.contains(expected),
            "missing {expected:?}:\n{rendered}"
        );
    }
    assert!(
        hits.targets().any(|target| target == &selected),
        "the selected replay frame stays clickable:\n{rendered}"
    );
}

#[test]
fn replay_board_navigation_cycles_views_and_walks_frames_in_recorded_order() {
    assert_eq!(BoardView::Dag.next(), BoardView::Flow);
    assert_eq!(BoardView::Flow.next(), BoardView::Replay);
    assert_eq!(BoardView::Replay.next(), BoardView::Fleet);
    assert_eq!(BoardView::Fleet.next(), BoardView::CostHealth);
    assert_eq!(BoardView::CostHealth.next(), BoardView::Dag);
    assert_eq!(BoardView::Dag.previous(), BoardView::CostHealth);

    let bytes = recording([
        (40, 0, Frame::Output(b"one".to_vec())),
        (41, 9, Frame::Resize { cols: 90, rows: 30 }),
        (42, 10, Frame::Output(b"two".to_vec())),
    ]);
    let state = replay_state(ReplayTimeline::decode(&bytes).expect("recording decodes"));

    assert_eq!(
        target_order(&state),
        vec![
            BoardTarget::ReplayFrame(40),
            BoardTarget::ReplayFrame(41),
            BoardTarget::ReplayFrame(42),
        ]
    );
}
