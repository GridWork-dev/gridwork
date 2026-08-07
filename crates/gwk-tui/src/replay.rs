//! Reading persisted PTY recordings into deterministic console replay frames.
//!
//! This module deliberately owns only the recording wire reader. It does not
//! link the PTY engine, render a screen, or wait for elapsed time; callers can
//! consume [`ReplayTimeline::frames`] at whatever deterministic pace they need.

/// The persisted recording signature and version emitted by `gwk-pty`.
const MAGIC: &[u8; 8] = b"GWKREC\0\x01";

/// One terminal change from a persisted recording.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayFrame {
    /// Bytes emitted by the child, retained without text decoding.
    Output {
        /// The recording position.
        seq: u64,
        /// Milliseconds since this recording began.
        elapsed_ms: u64,
        /// Opaque terminal bytes.
        bytes: Vec<u8>,
    },
    /// A terminal geometry change.
    Resize {
        /// The recording position.
        seq: u64,
        /// Milliseconds since this recording began.
        elapsed_ms: u64,
        /// Terminal width in cells.
        cols: u16,
        /// Terminal height in cells.
        rows: u16,
    },
}

/// A deterministic, ordered replay plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayTimeline {
    frames: Vec<ReplayFrame>,
}

/// Why persisted recording bytes cannot safely be replayed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReplayError {
    /// The bytes do not identify a version this reader supports.
    #[error("not a supported GridWork PTY recording")]
    Magic,
    /// A fixed-size field or event body ended before it was complete.
    #[error("recording ends mid-entry")]
    Truncated,
    /// The recording declares a type this reader does not recognize.
    #[error("unknown recording event tag {0}")]
    UnknownTag(u8),
    /// Sequences must be strictly increasing.
    #[error("sequence went from {previous} to {found}, which is out of order")]
    SequenceOutOfOrder { previous: u64, found: u64 },
    /// Recorded elapsed time must not move backwards.
    #[error("elapsed time went from {previous} to {found}, which is out of order")]
    ElapsedOutOfOrder { previous: u64, found: u64 },
    /// An output frame declares more bytes than remain in the stream.
    #[error("output length {declared} exceeds the {remaining} bytes remaining")]
    ImpossibleLength { declared: u32, remaining: usize },
}

impl ReplayTimeline {
    /// Decode the stable persisted recording encoding into ordered replay frames.
    pub fn decode(bytes: &[u8]) -> Result<Self, ReplayError> {
        let mut rest = bytes.strip_prefix(MAGIC).ok_or(ReplayError::Magic)?;
        let mut frames = Vec::new();
        let mut previous_seq = None;
        let mut previous_elapsed = None;

        while !rest.is_empty() {
            let seq = read_u64(&mut rest)?;
            if let Some(previous) = previous_seq
                && seq <= previous
            {
                return Err(ReplayError::SequenceOutOfOrder {
                    previous,
                    found: seq,
                });
            }

            let elapsed_ms = read_u64(&mut rest)?;
            if let Some(previous) = previous_elapsed
                && elapsed_ms < previous
            {
                return Err(ReplayError::ElapsedOutOfOrder {
                    previous,
                    found: elapsed_ms,
                });
            }

            let tag = take(&mut rest, 1)?[0];
            let frame = match tag {
                0 => {
                    let declared = read_u32(&mut rest)?;
                    let length =
                        usize::try_from(declared).map_err(|_| ReplayError::ImpossibleLength {
                            declared,
                            remaining: rest.len(),
                        })?;
                    if length > rest.len() {
                        return Err(ReplayError::ImpossibleLength {
                            declared,
                            remaining: rest.len(),
                        });
                    }
                    let bytes = take(&mut rest, length)?.to_vec();
                    ReplayFrame::Output {
                        seq,
                        elapsed_ms,
                        bytes,
                    }
                }
                1 => ReplayFrame::Resize {
                    seq,
                    elapsed_ms,
                    cols: read_u16(&mut rest)?,
                    rows: read_u16(&mut rest)?,
                },
                other => return Err(ReplayError::UnknownTag(other)),
            };

            previous_seq = Some(seq);
            previous_elapsed = Some(elapsed_ms);
            frames.push(frame);
        }

        Ok(Self { frames })
    }

    /// An empty replay timeline.
    pub const fn empty() -> Self {
        Self { frames: Vec::new() }
    }

    /// Frames in their persisted replay order.
    pub fn frames(&self) -> &[ReplayFrame] {
        &self.frames
    }
}

fn take<'a>(rest: &mut &'a [u8], length: usize) -> Result<&'a [u8], ReplayError> {
    if rest.len() < length {
        return Err(ReplayError::Truncated);
    }
    let (head, tail) = rest.split_at(length);
    *rest = tail;
    Ok(head)
}

fn read_u16(rest: &mut &[u8]) -> Result<u16, ReplayError> {
    let mut bytes = [0; 2];
    bytes.copy_from_slice(take(rest, 2)?);
    Ok(u16::from_le_bytes(bytes))
}

fn read_u32(rest: &mut &[u8]) -> Result<u32, ReplayError> {
    let mut bytes = [0; 4];
    bytes.copy_from_slice(take(rest, 4)?);
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(rest: &mut &[u8]) -> Result<u64, ReplayError> {
    let mut bytes = [0; 8];
    bytes.copy_from_slice(take(rest, 8)?);
    Ok(u64::from_le_bytes(bytes))
}
