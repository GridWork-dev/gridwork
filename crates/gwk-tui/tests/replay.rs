use gwk_tui::replay::{ReplayError, ReplayFrame, ReplayTimeline};

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
