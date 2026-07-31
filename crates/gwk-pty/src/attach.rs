//! Two clients on one session, and getting a returning one back in step.
//!
//! There are exactly two ways to bring a client current, and which one applies
//! is decided by the recording, not by the client:
//!
//! - **Fresh, or too far behind.** Send an [`Attach`]: the screen as VT bytes
//!   plus the sequence it reflects. Works from any starting point.
//! - **Behind but still inside the retained stream.** Send the entries after
//!   the client's sequence and let it [`replay`] them. Cheaper, and it keeps
//!   whatever scrollback the client already accumulated.
//!
//! [`Recording::since`](crate::record::Recording::since) is what decides:
//! it returns the entries when it can serve them and refuses when the gap has
//! been evicted. Falling back to a snapshot on that refusal is the whole
//! protocol.

use crate::Grid;
use crate::record::{Entry, Event, Recording};

/// Everything a client needs to start drawing from nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attach {
    /// VT sequences that reproduce the screen on a fresh grid of `cols`×`rows`.
    pub snapshot: Vec<u8>,
    /// The stream position this snapshot reflects. The client asks for
    /// everything after this number next time it falls behind.
    pub seq: u64,
    pub cols: u16,
    pub rows: u16,
}

impl Attach {
    /// Capture `grid` as it stands at stream position `seq`.
    pub fn capture(grid: &Grid, seq: u64) -> Option<Self> {
        let (cols, rows) = grid.size()?;
        Some(Self {
            snapshot: grid.snapshot_vt()?,
            seq,
            cols,
            rows,
        })
    }

    /// Build the grid this attachment describes.
    ///
    /// The grid is created at the snapshot's own dimensions rather than the
    /// client's: a snapshot is a picture of a specific geometry, and writing it
    /// into a differently-sized grid reflows it into something that never
    /// appeared on the original screen. A client that wants a different size
    /// resizes *after* attaching, which is a real resize and reflows the same
    /// way the session's own would.
    pub fn restore(&self) -> Option<Grid> {
        let mut grid = Grid::new(self.cols, self.rows)?;
        grid.write(&self.snapshot);
        Some(grid)
    }
}

/// Feed recorded entries back into a grid, in order.
///
/// Resizes are applied as resizes rather than written as bytes, because that is
/// what they were: a resize reaches a terminal through the terminal interface,
/// never through the stream of characters, so there is no byte sequence that
/// would reproduce one.
pub fn replay<'a>(entries: impl Iterator<Item = &'a Entry>, grid: &mut Grid) -> Option<()> {
    for entry in entries {
        match &entry.event {
            Event::Output(bytes) => grid.write(bytes),
            // Derivation: POSIX-WINSIZE — a terminal's window size is set by
            // `tcsetwinsize()` against the file descriptor, not by writing to
            // it, so there is no byte sequence a replay could emit that would
            // have the same effect. It has to be performed as a resize.
            Event::Resize { cols, rows } => grid.resize(*cols, *rows)?,
        }
    }
    Some(())
}

/// Bring a client holding `seq` up to date against `live`.
///
/// Returns the grid the client should now be showing. Prefers replay and falls
/// back to a snapshot when the stream no longer reaches back far enough.
pub fn catch_up(
    recording: &Recording,
    live: &Grid,
    client: &mut Grid,
    seq: u64,
) -> Option<CaughtUp> {
    match recording.since(seq) {
        Ok(entries) => {
            replay(entries, client)?;
            Some(CaughtUp::Replayed {
                through: recording.latest_seq().unwrap_or(seq),
            })
        }
        Err(_) => {
            let seq = recording.latest_seq().unwrap_or(seq);
            let attach = Attach::capture(live, seq)?;
            *client = attach.restore()?;
            Some(CaughtUp::Snapshotted { at: seq })
        }
    }
}

/// How a client was brought current.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaughtUp {
    /// The retained stream covered the gap.
    Replayed { through: u64 },
    /// The gap had been evicted, so the client was reset to a snapshot.
    Snapshotted { at: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two grids show the same screen, cursor included.
    ///
    /// Text and cursor, not `is_ok()`: a replay that silently produced a blank
    /// screen would satisfy any assertion that only checked for success, and a
    /// caret in the wrong place is a visible defect a text-only comparison
    /// misses.
    #[track_caller]
    fn assert_same_screen(a: &Grid, b: &Grid) {
        assert_eq!(a.text(), b.text(), "screen contents diverged");
        assert_eq!(a.cursor(), b.cursor(), "cursor position diverged");
        assert_eq!(a.size(), b.size(), "geometry diverged");
    }

    /// Drive a grid and record what drove it, so the two cannot drift apart.
    struct Session {
        grid: Grid,
        recording: Recording,
        clock: u64,
    }

    impl Session {
        fn new(cols: u16, rows: u16, scrollback: usize, cap: usize) -> Self {
            Self {
                grid: Grid::with_scrollback(cols, rows, scrollback).expect("valid grid"),
                recording: Recording::new(cap),
                clock: 0,
            }
        }
        fn write(&mut self, bytes: &[u8]) -> u64 {
            self.clock += 1;
            self.grid.write(bytes);
            self.recording
                .record(self.clock, Event::Output(bytes.to_vec()))
        }
        fn resize(&mut self, cols: u16, rows: u16) -> u64 {
            self.clock += 1;
            self.grid.resize(cols, rows).expect("valid size");
            self.recording
                .record(self.clock, Event::Resize { cols, rows })
        }
    }

    #[test]
    fn a_second_client_attaches_to_the_screen_already_on_show() {
        let mut session = Session::new(20, 5, 100, 100);
        session.write(b"first\r\nsecond\r\nthird");
        let seq = session.recording.latest_seq().expect("recorded");

        let attached = Attach::capture(&session.grid, seq)
            .expect("capture")
            .restore()
            .expect("restore");

        assert_same_screen(&session.grid, &attached);
    }

    #[test]
    fn a_returning_client_replays_the_gap_including_a_resize() {
        let mut session = Session::new(20, 5, 100, 100);
        session.write(b"before\r\n");

        // The client attaches here, then stops listening.
        let seq = session.recording.latest_seq().expect("recorded");
        let mut client = Attach::capture(&session.grid, seq)
            .expect("capture")
            .restore()
            .expect("restore");
        assert_same_screen(&session.grid, &client);

        // While it is away: more output, a resize, and more output after it.
        // The resize is the case a byte-only stream gets wrong — it never
        // travels through the pty, so replaying only the output would leave
        // the client reflowing every later line against 20 columns.
        session.write(b"during\r\n");
        session.resize(34, 8);
        session.write(b"after the resize, this line is long enough to wrap at twenty\r\n");

        let outcome =
            catch_up(&session.recording, &session.grid, &mut client, seq).expect("catch up");

        assert_eq!(
            outcome,
            CaughtUp::Replayed {
                through: session.recording.latest_seq().expect("recorded")
            }
        );
        assert_same_screen(&session.grid, &client);
    }

    #[test]
    fn a_client_past_the_retention_horizon_is_snapshotted_instead() {
        // A cap of 4 with far more than 4 events, so the client's position is
        // genuinely gone rather than merely old.
        let mut session = Session::new(20, 5, 100, 4);
        session.write(b"oldest\r\n");
        let stale = session.recording.latest_seq().expect("recorded");
        let mut client = Attach::capture(&session.grid, stale)
            .expect("capture")
            .restore()
            .expect("restore");

        for i in 0..12 {
            session.write(format!("line {i}\r\n").as_bytes());
        }
        assert!(
            session.recording.since(stale).is_err(),
            "the test is meaningless unless the gap really was evicted"
        );

        let outcome =
            catch_up(&session.recording, &session.grid, &mut client, stale).expect("catch up");

        assert_eq!(
            outcome,
            CaughtUp::Snapshotted {
                at: session.recording.latest_seq().expect("recorded")
            }
        );
        assert_same_screen(&session.grid, &client);
    }

    #[test]
    fn a_snapshot_carries_history_but_loses_its_oldest_row() {
        // A small scrollback and far more lines than it, so history has
        // certainly been evicted on the live grid.
        //
        // The assertion is "fewer rows than were written", not an exact count.
        // `max_scrollback` is documented as a number of lines but does not
        // behave as an exact one — a cap of 3 retains 17 rows, because the
        // history is allocated in pages and a cap below a page still gets a
        // page. Asserting the number would be asserting an allocator detail.
        const LINES: usize = 500;
        let mut session = Session::new(20, 4, 3, LINES + 10);
        for i in 0..LINES {
            session.write(format!("line {i}\r\n").as_bytes());
        }
        let live_scrollback = session.grid.scrollback_rows().expect("scrollback");
        assert!(
            live_scrollback > 0 && live_scrollback < LINES,
            "expected eviction: {LINES} lines written, {live_scrollback} retained"
        );

        let seq = session.recording.latest_seq().expect("recorded");
        let attached = Attach::capture(&session.grid, seq)
            .expect("capture")
            .restore()
            .expect("restore");

        // The visible screen matches, which is what a client draws.
        assert_same_screen(&session.grid, &attached);

        // The history comes across too, which is the opposite of the natural
        // assumption — "snapshot" sounds like a picture of the visible screen,
        // and a client restored from one would then be unable to scroll up
        // into anything from before it attached. It can.
        //
        // It arrives exactly one row short, every time, at every size tried.
        // That row is the oldest one the session still held. This is pinned
        // rather than papered over with a range, because a range would also
        // pass the day the dump starts losing half the history. A client that
        // needs byte-exact history replays the stream instead, which is the
        // other branch of `catch_up` and is asserted above.
        assert_eq!(
            attached.scrollback_rows(),
            Some(live_scrollback - 1),
            "a snapshot should carry the retained history bar its oldest row"
        );
    }

    #[test]
    fn replaying_the_whole_stream_rebuilds_the_session_from_nothing() {
        // The strongest form of the claim: no snapshot at all, just the
        // recording, and the result has to match cell for cell.
        let mut session = Session::new(24, 6, 50, 500);
        session.write(b"\x1b[1;1Halpha");
        session.write(b"\x1b[3;5Hbravo");
        session.resize(40, 10);
        session.write(b"\x1b[6;2Hcharlie\r\ndelta");
        session.resize(18, 4);
        session.write(b"\x1b[2;1Hecho");

        let mut rebuilt = Grid::with_scrollback(24, 6, 50).expect("valid grid");
        replay(session.recording.entries(), &mut rebuilt).expect("replay");

        assert_same_screen(&session.grid, &rebuilt);
    }
}
