//! The delta stream: everything that changed the terminal, in order, with the
//! sequence numbers a reattaching client counts from.
//!
//! # Why the stream is input, not pixels
//!
//! A "render-state delta" could mean either the rows that changed on screen or
//! the input that changed them. This module records the input, because it is
//! the only one of the two that can be *replayed*: libghostty-vt will parse
//! bytes into a screen, and offers nothing that writes a screen back out of
//! row contents. Recording rendered rows would produce a stream that can be
//! drawn once and never used to rebuild the terminal it came from.
//!
//! The rows-that-changed view still exists — see [`crate::render`] — and is
//! stamped with the sequence numbers issued here, so there is exactly one
//! numbering in the crate rather than two that could disagree.
//!
//! # Where a recording ends up
//!
//! A persisted recording is an `Evidence` row with `kind = "pty_recording"`,
//! whose `ref` is a `sha256:` blob address over exactly the bytes
//! [`Recording::encode`] produces, with `digest` and `byte_size` populated.
//! There is no `pty_recording` table and this crate does not talk to a
//! database: it produces the bytes and the digest, and storage is the caller's.
//! Retention of the stored blob rides the blob verbs that already ship; the cap
//! here is the in-memory one, which is a different question with a different
//! answer.

use std::collections::VecDeque;

use sha2::{Digest, Sha256};

/// The scheme a blob address carries, matching `gwk-domain`'s.
///
/// Spelled out rather than imported: this crate needs eight characters of that
/// crate's vocabulary and none of its types, and a dependency edge from the PTY
/// engine to the contract domain would be a real one for a string constant.
/// The pairing is asserted by `digest_is_a_blob_address` below.
const BLOB_ADDRESS_SCHEME: &str = "sha256:";

/// The file signature [`Recording::encode`] writes and [`Recording::decode`]
/// requires. The trailing byte is a format version.
const MAGIC: &[u8; 8] = b"GWKREC\0\x01";

/// Something that changed the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    // Derivation: ECMA-48 §5.4 — a control sequence runs from CSI through its
    // final byte and a read can end anywhere inside one, so a chunk is stored
    // whole and replayed in order rather than normalised, re-split, or joined
    // with its neighbours. The parser is the state machine that spans those
    // boundaries; a recording that tidied them would be replaying a stream the
    // child never wrote.
    /// Bytes the child wrote, exactly as they were read.
    Output(Vec<u8>),
    // Derivation: POSIX-WINSIZE — the window size lives in a `winsize` struct
    // set by `tcsetwinsize()` against a file descriptor, which is a channel
    // separate from the bytes a process writes to that same descriptor. So it
    // cannot be recovered from the byte stream and has to ride the recording as
    // its own event; a replay that dropped it would reflow every later line
    // against the wrong width.
    /// The window changed size.
    Resize { cols: u16, rows: u16 },
}

/// One event, numbered and time-stamped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Position in the stream. Strictly increasing, never reused.
    pub seq: u64,
    /// Milliseconds since the recording started, for replaying at speed.
    pub offset_ms: u64,
    pub event: Event,
}

/// Why a reattach could not be served from the stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("sequence {asked} is older than the {oldest} still retained; send a snapshot instead")]
pub struct Evicted {
    pub asked: u64,
    pub oldest: u64,
}

/// Why a recording could not be read back.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("not a gwk recording, or a version this build does not read")]
    Magic,
    #[error("the stream ends mid-entry")]
    Truncated,
    #[error("unknown event tag {0}")]
    Tag(u8),
    #[error("sequence went from {previous} to {found}, which is backwards")]
    OutOfOrder { previous: u64, found: u64 },
}

/// An ordered, sequence-numbered, capped stream of terminal deltas.
pub struct Recording {
    entries: VecDeque<Entry>,
    next_seq: u64,
    cap: usize,
}

impl Recording {
    /// A recording retaining at most `cap` entries.
    ///
    /// The cap is a parameter and not a constant on purpose: the right number
    /// depends on what a real session's traffic looks like, and nothing has
    /// measured one yet. Picking a default here would freeze a guess into the
    /// type.
    pub fn new(cap: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            next_seq: 0,
            cap,
        }
    }

    /// Append an event, returning the sequence number it was given.
    ///
    /// Sequence numbers keep climbing across eviction. They number the stream,
    /// not the buffer, so a client holding seq 900 after entries 0–899 are gone
    /// still gets a correct answer from [`since`](Self::since) — an `Evicted`
    /// error rather than a wrong slice.
    pub fn record(&mut self, offset_ms: u64, event: Event) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        self.entries.push_back(Entry {
            seq,
            offset_ms,
            event,
        });
        while self.entries.len() > self.cap {
            self.entries.pop_front();
        }
        seq
    }

    /// The sequence number of the most recent entry, or `None` when empty.
    pub fn latest_seq(&self) -> Option<u64> {
        self.entries.back().map(|e| e.seq)
    }

    /// The oldest sequence number still retained.
    pub fn oldest_seq(&self) -> Option<u64> {
        self.entries.front().map(|e| e.seq)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Everything after `seq`, for a client that already has through `seq`.
    ///
    /// Returns [`Evicted`] when the next entry the caller needs has already
    /// been dropped. That refusal is the point of the type: serving the
    /// remaining suffix would silently skip the gap, and the client would
    /// render a screen that never existed while believing it was current.
    pub fn since(&self, seq: u64) -> Result<impl Iterator<Item = &Entry>, Evicted> {
        if let Some(oldest) = self.oldest_seq() {
            // `seq + 1` is the first entry the caller still needs. If the
            // buffer starts after that, the entries between are gone.
            if seq + 1 < oldest {
                return Err(Evicted { asked: seq, oldest });
            }
        }
        Ok(self.entries.iter().filter(move |e| e.seq > seq))
    }

    /// Everything retained, oldest first.
    pub fn entries(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter()
    }

    /// Serialize to the bytes a blob address is taken over.
    ///
    /// Hand-rolled and length-prefixed rather than JSON: the payload is mostly
    /// arbitrary child output, which JSON cannot hold without base64 inflating
    /// it by a third, and a recording is the one artifact here that is stored
    /// at volume.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::from(*MAGIC);
        for entry in &self.entries {
            out.extend_from_slice(&entry.seq.to_le_bytes());
            out.extend_from_slice(&entry.offset_ms.to_le_bytes());
            match &entry.event {
                Event::Output(bytes) => {
                    out.push(0);
                    // A u32 length caps one entry at 4 GiB. A single PTY read
                    // is bounded by the read buffer, three orders of magnitude
                    // under that, so the cast cannot truncate anything this
                    // crate produces.
                    let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
                    out.extend_from_slice(&len.to_le_bytes());
                    out.extend_from_slice(&bytes[..len as usize]);
                }
                Event::Resize { cols, rows } => {
                    out.push(1);
                    out.extend_from_slice(&cols.to_le_bytes());
                    out.extend_from_slice(&rows.to_le_bytes());
                }
            }
        }
        out
    }

    /// Read back what [`encode`](Self::encode) wrote.
    ///
    /// `cap` is the retention of the *new* recording and is not stored in the
    /// stream: it describes how much a process is willing to hold, which is a
    /// property of the reader, not of the bytes.
    pub fn decode(bytes: &[u8], cap: usize) -> Result<Self, DecodeError> {
        let mut rest = bytes.strip_prefix(MAGIC).ok_or(DecodeError::Magic)?;
        let mut entries = VecDeque::new();
        let mut previous: Option<u64> = None;

        fn take<'a>(rest: &mut &'a [u8], n: usize) -> Result<&'a [u8], DecodeError> {
            if rest.len() < n {
                return Err(DecodeError::Truncated);
            }
            let (head, tail) = rest.split_at(n);
            *rest = tail;
            Ok(head)
        }
        fn u64_le(rest: &mut &[u8]) -> Result<u64, DecodeError> {
            Ok(u64::from_le_bytes(
                take(rest, 8)?.try_into().expect("8 bytes"),
            ))
        }
        fn u16_le(rest: &mut &[u8]) -> Result<u16, DecodeError> {
            Ok(u16::from_le_bytes(
                take(rest, 2)?.try_into().expect("2 bytes"),
            ))
        }

        while !rest.is_empty() {
            let seq = u64_le(&mut rest)?;
            // Order is what `since` binary-searches on conceptually and what
            // every replay depends on. A stream that arrives out of order is
            // corrupt, and accepting it would push the failure into whichever
            // reattach happened to land on the reordered pair.
            if let Some(previous) = previous
                && seq <= previous
            {
                return Err(DecodeError::OutOfOrder {
                    previous,
                    found: seq,
                });
            }
            previous = Some(seq);

            let offset_ms = u64_le(&mut rest)?;
            let event = match take(&mut rest, 1)?[0] {
                0 => {
                    let len = u64::from(u32::from_le_bytes(
                        take(&mut rest, 4)?.try_into().expect("4 bytes"),
                    ));
                    let len = usize::try_from(len).map_err(|_| DecodeError::Truncated)?;
                    Event::Output(take(&mut rest, len)?.to_vec())
                }
                1 => Event::Resize {
                    cols: u16_le(&mut rest)?,
                    rows: u16_le(&mut rest)?,
                },
                other => return Err(DecodeError::Tag(other)),
            };
            entries.push_back(Entry {
                seq,
                offset_ms,
                event,
            });
        }

        while entries.len() > cap {
            entries.pop_front();
        }
        let next_seq = entries.back().map_or(0, |e| e.seq + 1);
        Ok(Self {
            entries,
            next_seq,
            cap,
        })
    }

    /// The blob address of this recording's encoded bytes.
    ///
    /// This is the value that goes in an `Evidence` row's `ref`; `byte_size` is
    /// the length of [`encode`](Self::encode)'s output.
    pub fn digest(&self) -> String {
        let digest: [u8; 32] = Sha256::digest(self.encode()).into();
        let mut out = String::with_capacity(BLOB_ADDRESS_SCHEME.len() + 64);
        out.push_str(BLOB_ADDRESS_SCHEME);
        for byte in digest {
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(s: &str) -> Event {
        Event::Output(s.as_bytes().to_vec())
    }

    #[test]
    fn sequence_numbers_only_ever_climb() {
        let mut rec = Recording::new(4);
        // More events than the cap, so eviction happens mid-test and the
        // numbering has to survive it.
        let issued: Vec<u64> = (0..10).map(|i| rec.record(i * 10, out("x"))).collect();

        assert!(
            issued.windows(2).all(|w| w[1] > w[0]),
            "issued {issued:?} is not strictly increasing"
        );
        assert_eq!(rec.len(), 4, "the cap should have evicted the rest");
        assert_eq!(rec.oldest_seq(), Some(6));
        assert_eq!(rec.latest_seq(), Some(9));

        let retained: Vec<u64> = rec.entries().map(|e| e.seq).collect();
        assert!(
            retained.windows(2).all(|w| w[1] > w[0]),
            "retained {retained:?} is not strictly increasing"
        );
    }

    #[test]
    fn since_serves_the_suffix_a_client_is_missing() {
        let mut rec = Recording::new(100);
        for i in 0..5 {
            rec.record(i, out("x"));
        }
        let got: Vec<u64> = rec
            .since(2)
            .expect("nothing evicted")
            .map(|e| e.seq)
            .collect();
        assert_eq!(got, vec![3, 4]);

        // A client that is already current gets nothing, not an error.
        assert_eq!(rec.since(4).expect("current").count(), 0);
    }

    #[test]
    fn since_refuses_rather_than_serving_a_stream_with_a_hole_in_it() {
        let mut rec = Recording::new(3);
        for i in 0..10 {
            rec.record(i, out("x"));
        }
        // Retains 7,8,9. A client holding 2 needs 3 onward, and 3-6 are gone.
        assert_eq!(
            rec.since(2).err(),
            Some(Evicted {
                asked: 2,
                oldest: 7
            })
        );
        // The boundary: holding 6 means the next needed is 7, which is present.
        assert_eq!(rec.since(6).expect("6 is servable").count(), 3);
    }

    #[test]
    fn a_recording_survives_the_round_trip() {
        let mut rec = Recording::new(100);
        rec.record(0, out("hello"));
        rec.record(
            12,
            Event::Resize {
                cols: 100,
                rows: 30,
            },
        );
        // Arbitrary bytes, not text: the payload is whatever a child wrote.
        rec.record(40, Event::Output(vec![0x00, 0xff, 0x1b, 0x5b, 0xc3]));
        rec.record(41, Event::Output(Vec::new()));

        let bytes = rec.encode();
        let back = Recording::decode(&bytes, 100).expect("decode");

        assert_eq!(
            back.entries().cloned().collect::<Vec<_>>(),
            rec.entries().cloned().collect::<Vec<_>>()
        );
        assert_eq!(back.encode(), bytes, "re-encoding should be byte-identical");
        assert_eq!(back.digest(), rec.digest());
        // A decoded recording keeps numbering where the stream left off, so
        // appending to a resumed recording cannot reuse a sequence number.
        let mut back = back;
        assert_eq!(back.record(50, out("more")), 4);
    }

    #[test]
    fn digest_is_a_blob_address() {
        let digest = Recording::new(1).digest();
        let hex = digest
            .strip_prefix(BLOB_ADDRESS_SCHEME)
            .expect("digest should carry the blob-address scheme");
        assert_eq!(hex.len(), 64, "sha-256 is 32 bytes of lowercase hex");
        assert!(
            hex.chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        );
    }

    /// `Recording` holds no `Debug`, so assertions go through the error side.
    fn decode_err(bytes: &[u8]) -> DecodeError {
        match Recording::decode(bytes, 10) {
            Err(e) => e,
            Ok(_) => panic!("expected a refusal, got a recording"),
        }
    }

    #[test]
    fn a_corrupt_stream_is_refused_rather_than_half_read() {
        let mut rec = Recording::new(10);
        rec.record(0, out("hello"));
        rec.record(1, Event::Resize { cols: 80, rows: 24 });
        let good = rec.encode();

        assert_eq!(decode_err(b"not a recording"), DecodeError::Magic);

        // Magic alone is a valid empty recording, so the truncation has to cut
        // into an entry to be a truncation.
        assert!(Recording::decode(MAGIC, 10).is_ok());
        assert_eq!(decode_err(&good[..good.len() - 1]), DecodeError::Truncated);
        assert_eq!(decode_err(&good[..MAGIC.len() + 4]), DecodeError::Truncated);

        // The tag byte sits after seq (8) and offset (8).
        let mut bad_tag = good.clone();
        bad_tag[MAGIC.len() + 16] = 9;
        assert_eq!(decode_err(&bad_tag), DecodeError::Tag(9));

        // Rewrite the first entry's seq to sit at or above the second's.
        let mut backwards = good.clone();
        backwards[MAGIC.len()..MAGIC.len() + 8].copy_from_slice(&7u64.to_le_bytes());
        assert_eq!(
            decode_err(&backwards),
            DecodeError::OutOfOrder {
                previous: 7,
                found: 1
            }
        );
    }
}
