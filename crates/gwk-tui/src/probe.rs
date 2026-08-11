//! Measuring what a terminal actually does with the inventory.
//!
//! The verdict lives in [`gwk_theme::probe`], which is pure and cannot touch a
//! terminal. This is the half that writes bytes and reads the reply, which is
//! why it is here and not there.
//!
//! # How the measurement works
//!
//! For each inventory codepoint: park the cursor at column 1, write the glyph,
//! then ask the terminal where the cursor ended up (`ESC [ 6 n`, Device Status
//! Report). The reply is `ESC [ <row> ; <col> R`, and `col - 1` is how many
//! cells the terminal decided that glyph occupies.
//!
//! This asks **the terminal that is drawing**, which is the only thing that
//! matters for a client designed to attach from elsewhere. Inspecting the local
//! machine's fonts would answer a question about the wrong computer whenever the
//! session is remote.
//!
//! # Why it takes a reader and a writer rather than opening the tty
//!
//! So it can be tested. A probe wired directly to stdout could only be exercised
//! by a human looking at a terminal, which is the thing this whole module exists
//! to stop relying on.

use std::io::{self, Read, Write};
#[cfg(target_os = "linux")]
use std::time::{Duration, Instant};

#[cfg(not(target_os = "linux"))]
use crossterm::cursor::position;
use gwk_theme::probe::{CellReport, ProbeOutcome, evaluate, inventory_codepoints};
#[cfg(target_os = "linux")]
use rustix::event::{PollFd, PollFlags, Timespec, poll};
#[cfg(target_os = "linux")]
use std::os::fd::AsFd as _;

/// How many bytes we will read chasing a single reply before giving up.
///
/// A terminal that answers at all answers within a few bytes. This bounds reply
/// parsing; [`probe_terminal`] separately bounds how long production waits.
const REPLY_BUDGET: usize = 32;
#[cfg(target_os = "linux")]
const REPLY_TIMEOUT: Duration = Duration::from_millis(250);
/// How long a straggling reply gets once a retry has already answered. Short
/// on purpose: the reply we did not read was in flight before the retry, so it
/// either lands at once or was never coming.
#[cfg(target_os = "linux")]
const STRAGGLER_WINDOW: Duration = Duration::from_millis(50);

/// Ask the terminal how wide one glyph is.
///
/// Returns `None` when the terminal does not answer intelligibly. `None` is not
/// an error: it becomes an unanswered codepoint, and silence degrades.
fn measure_one<W: Write, R: Read>(out: &mut W, inp: &mut R, glyph: char) -> Option<u16> {
    // Column 1, write the glyph, ask where we are.
    write!(out, "\r{glyph}\x1b[6n").ok()?;
    out.flush().ok()?;

    let mut buf = Vec::with_capacity(REPLY_BUDGET);
    let mut byte = [0u8; 1];
    while buf.len() < REPLY_BUDGET {
        match inp.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                buf.push(byte[0]);
                if byte[0] == b'R' {
                    break;
                }
            }
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }

    parse_cpr(&buf).map(|col| col.saturating_sub(1))
}

/// Pull the column out of a `ESC [ row ; col R` reply.
///
/// Tolerant of leading noise, because a terminal may have queued output of its
/// own before the reply; strict about the shape once it starts, because a
/// half-parsed reply is worse than no reply.
fn parse_cpr(buf: &[u8]) -> Option<u16> {
    let start = buf.windows(2).position(|w| w == b"\x1b[")? + 2;
    let end = buf[start..].iter().position(|&b| b == b'R')? + start;
    let body = std::str::from_utf8(&buf[start..end]).ok()?;
    let (_row, col) = body.split_once(';')?;
    col.trim().parse::<u16>().ok()
}

/// Measure every inventory codepoint and return the verdict.
///
/// The terminal must already be in raw mode — otherwise the reply is line
/// buffered and will not arrive until the user presses enter. Putting it in raw
/// mode is the caller's job, because the caller is the one that has to put it
/// back.
pub fn probe<W: Write, R: Read>(out: &mut W, inp: &mut R) -> ProbeOutcome {
    let mut reports = Vec::new();
    for glyph in inventory_codepoints() {
        if let Some(cells) = measure_one(out, inp, glyph) {
            reports.push(CellReport { glyph, cells });
        }
    }
    // Leave the line as we found it: the probe wrote glyphs to it.
    let _ = write!(out, "\r\x1b[2K");
    let _ = out.flush();

    evaluate(&reports)
}

/// Probe the active terminal through a bounded cursor-position query. Linux
/// waits at most 250ms through `poll` and asks a silent glyph a second time
/// before concluding anything; other targets use crossterm's bounded position
/// query. A glyph that stays silent twice degrades the rest of the inventory.
pub fn probe_terminal<W: Write>(out: &mut W) -> ProbeOutcome {
    #[cfg(target_os = "linux")]
    {
        let stdin = io::stdin();
        let mut input = stdin.lock();
        probe_timed(out, &mut input)
    }
    #[cfg(not(target_os = "linux"))]
    probe_positions(out, || position().map(|(column, _row)| column))
}

#[cfg(target_os = "linux")]
fn probe_timed<W, R>(out: &mut W, input: &mut R) -> ProbeOutcome
where
    W: Write,
    R: Read + std::os::fd::AsFd,
{
    let mut reports = Vec::new();
    for glyph in inventory_codepoints() {
        // One retry per glyph: under a multiplexer the first reply routinely
        // lands just past the 250ms window, and treating that one late reply
        // as terminal silence used to degrade every glyph after it. A glyph
        // that stays silent twice still ends the probe — a terminal that
        // answers nothing would otherwise cost the full inventory in
        // timeouts.
        let measured = match measure_one_timed(out, input, glyph) {
            Some(cells) => Some(cells),
            None => {
                let retried = measure_one_timed(out, input, glyph);
                if retried.is_some() {
                    // Two asks were in flight. Both printed the same glyph at
                    // column one, so the reply just parsed is a true
                    // measurement whichever ask it answers — but the other
                    // answer may still be coming, and the next glyph would
                    // read it as its own.
                    discard_reply(input, STRAGGLER_WINDOW);
                }
                retried
            }
        };
        if let Some(cells) = measured {
            reports.push(CellReport { glyph, cells });
        } else {
            break;
        }
    }
    let _ = write!(out, "\r\x1b[2K");
    let _ = out.flush();
    evaluate(&reports)
}

#[cfg(target_os = "linux")]
fn measure_one_timed<W, R>(out: &mut W, input: &mut R, glyph: char) -> Option<u16>
where
    W: Write,
    R: Read + std::os::fd::AsFd,
{
    write!(out, "\r{glyph}\x1b[6n").ok()?;
    out.flush().ok()?;
    let deadline = Instant::now() + REPLY_TIMEOUT;
    let mut buf = Vec::with_capacity(REPLY_BUDGET);
    let mut chunk = [0u8; REPLY_BUDGET];
    while buf.len() < REPLY_BUDGET {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let timeout = Timespec::try_from(remaining).ok()?;
        let mut ready = [PollFd::from_borrowed_fd(input.as_fd(), PollFlags::IN)];
        if poll(&mut ready, Some(&timeout)).ok()? == 0 {
            break;
        }
        // Take the whole reply in one read rather than a byte at a time. The
        // wait watches the descriptor while the read goes through whatever
        // the reader is — and a buffering reader (`io::stdin().lock()` is
        // one) empties the descriptor into userspace on the first read, so a
        // byte-at-a-time loop would then wait on a queue that is already
        // drained and call an answered terminal silent.
        let room = REPLY_BUDGET - buf.len();
        match input.read(&mut chunk[..room]) {
            Ok(0) => break,
            Ok(read) => {
                buf.extend_from_slice(&chunk[..read]);
                if buf.contains(&b'R') {
                    break;
                }
            }
            Err(ref error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    parse_cpr(&buf).map(|column| column.saturating_sub(1))
}

/// Swallow one straggling reply, waiting up to `window` for it.
#[cfg(target_os = "linux")]
fn discard_reply<R>(input: &mut R, window: Duration)
where
    R: Read + std::os::fd::AsFd,
{
    let deadline = Instant::now() + window;
    let mut chunk = [0u8; REPLY_BUDGET];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return;
        }
        let Ok(timeout) = Timespec::try_from(remaining) else {
            return;
        };
        let mut ready = [PollFd::from_borrowed_fd(input.as_fd(), PollFlags::IN)];
        match poll(&mut ready, Some(&timeout)) {
            Ok(count) if count > 0 => {}
            _ => return,
        }
        match input.read(&mut chunk) {
            Ok(read) if read > 0 => {
                if chunk[..read].contains(&b'R') {
                    return;
                }
            }
            _ => return,
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn probe_positions<W, F>(out: &mut W, mut cursor_column: F) -> ProbeOutcome
where
    W: Write,
    F: FnMut() -> io::Result<u16>,
{
    let mut reports = Vec::new();
    for glyph in inventory_codepoints() {
        if write!(out, "\r{glyph}").and_then(|()| out.flush()).is_err() {
            break;
        }
        let Ok(cells) = cursor_column() else {
            break;
        };
        reports.push(CellReport { glyph, cells });
    }
    let _ = write!(out, "\r\x1b[2K");
    let _ = out.flush();
    evaluate(&reports)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gwk_theme::marks::GlyphSet;

    /// A terminal that answers every query with the same column.
    struct Fake {
        replies: Vec<u8>,
        pos: usize,
    }

    impl Fake {
        fn always(col: u16, n: usize) -> Self {
            let mut replies = Vec::new();
            for _ in 0..n {
                replies.extend_from_slice(format!("\x1b[1;{col}R").as_bytes());
            }
            Fake { replies, pos: 0 }
        }
        fn silent() -> Self {
            Fake {
                replies: Vec::new(),
                pos: 0,
            }
        }
    }

    impl Read for Fake {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pos >= self.replies.len() {
                return Ok(0);
            }
            buf[0] = self.replies[self.pos];
            self.pos += 1;
            Ok(1)
        }
    }

    #[test]
    fn probe_cpr_parses_the_column() {
        assert_eq!(parse_cpr(b"\x1b[12;2R"), Some(2));
        assert_eq!(parse_cpr(b"\x1b[1;3R"), Some(3));
    }

    #[test]
    fn probe_cpr_tolerates_noise_before_the_reply() {
        assert_eq!(parse_cpr(b"junk\x1b[1;2R"), Some(2));
    }

    #[test]
    fn probe_cpr_refuses_a_malformed_reply_rather_than_guessing() {
        assert_eq!(parse_cpr(b""), None);
        assert_eq!(parse_cpr(b"\x1b[1;R"), None);
        assert_eq!(parse_cpr(b"\x1b[nonsense R"), None);
        // Truncated: started but never terminated.
        assert_eq!(parse_cpr(b"\x1b[1;2"), None);
    }

    #[test]
    fn probe_a_terminal_reporting_column_2_is_one_cell_and_passes() {
        let n = inventory_codepoints().len();
        let mut out = Vec::new();
        let mut inp = Fake::always(2, n);
        let outcome = probe(&mut out, &mut inp);
        assert_eq!(outcome, ProbeOutcome::UnicodeSafe);
        assert_eq!(outcome.glyph_set(), GlyphSet::Unicode);
    }

    #[test]
    fn probe_a_terminal_reporting_column_3_is_two_cells_and_degrades() {
        let n = inventory_codepoints().len();
        let mut out = Vec::new();
        let mut inp = Fake::always(3, n);
        let outcome = probe(&mut out, &mut inp);
        assert!(matches!(outcome, ProbeOutcome::Sheared(_)));
        assert_eq!(outcome.glyph_set(), GlyphSet::Ascii);
    }

    #[test]
    fn probe_a_silent_terminal_degrades_rather_than_hanging_or_passing() {
        let mut out = Vec::new();
        let mut inp = Fake::silent();
        let outcome = probe(&mut out, &mut inp);
        assert!(matches!(outcome, ProbeOutcome::Inconclusive(_)));
        assert_eq!(outcome.glyph_set(), GlyphSet::Ascii);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn terminal_probe_stops_after_the_first_unanswered_position() {
        let mut out = Vec::new();
        let mut calls = 0;
        let outcome = probe_positions(&mut out, || {
            calls += 1;
            Err(io::Error::new(io::ErrorKind::TimedOut, "silent terminal"))
        });
        assert_eq!(calls, 1);
        assert_eq!(outcome.glyph_set(), GlyphSet::Ascii);
    }

    /// A terminal on the other end of a socket: answers every `ESC [ 6 n`
    /// with column 2, after staying silent for the first `silent` asks.
    #[cfg(target_os = "linux")]
    fn spawn_responder(
        mut stream: std::os::unix::net::UnixStream,
        silent: usize,
    ) -> std::thread::JoinHandle<()> {
        use std::io::Write as _;
        std::thread::spawn(move || {
            let mut pending = Vec::new();
            let mut chunk = [0u8; 64];
            let mut asked = 0usize;
            loop {
                let read = match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => return,
                    Ok(read) => read,
                };
                pending.extend_from_slice(&chunk[..read]);
                while let Some(at) = pending.windows(4).position(|w| w == b"\x1b[6n") {
                    pending.drain(..at + 4);
                    asked += 1;
                    if asked <= silent {
                        continue;
                    }
                    if stream.write_all(b"\x1b[1;2R").is_err() {
                        return;
                    }
                }
            }
        })
    }

    /// The production Linux path: a terminal whose FIRST reply misses the
    /// 250ms window entirely. Without the retry, that one silence ended the
    /// loop and every remaining codepoint landed in Inconclusive; with it,
    /// the probe asks again and the inventory survives.
    #[cfg(target_os = "linux")]
    #[test]
    fn timed_probe_retries_a_silent_glyph_before_degrading_the_inventory() {
        use std::os::unix::net::UnixStream;

        let (probe_side, responder_side) = UnixStream::pair().expect("socketpair");
        let mut out = probe_side.try_clone().expect("clone probe side");
        let mut input = probe_side;
        let responder = spawn_responder(responder_side, 1);

        let outcome = probe_timed(&mut out, &mut input);
        drop(out);
        drop(input);
        responder.join().expect("responder thread");
        assert_eq!(outcome, ProbeOutcome::UnicodeSafe);
    }

    /// Production hands the probe `io::stdin().lock()`, which buffers: the
    /// first read empties the descriptor into userspace, so waiting on the
    /// descriptor for the rest of the reply waits on a queue that is already
    /// drained. Read a byte at a time and this answers-instantly terminal
    /// reads as mute — and since the degradation now speaks, it says so out
    /// loud and wrongly.
    #[cfg(target_os = "linux")]
    #[test]
    fn timed_probe_reads_a_whole_reply_through_a_buffering_reader() {
        use std::io::BufReader;
        use std::os::fd::{AsFd, BorrowedFd};
        use std::os::unix::net::UnixStream;

        struct Buffered(BufReader<UnixStream>);
        impl Read for Buffered {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                self.0.read(buf)
            }
        }
        impl AsFd for Buffered {
            fn as_fd(&self) -> BorrowedFd<'_> {
                self.0.get_ref().as_fd()
            }
        }

        let (probe_side, responder_side) = UnixStream::pair().expect("socketpair");
        let mut out = probe_side.try_clone().expect("clone probe side");
        let mut input = Buffered(BufReader::new(probe_side));
        let responder = spawn_responder(responder_side, 0);

        let outcome = probe_timed(&mut out, &mut input);
        drop(out);
        drop(input);
        responder.join().expect("responder thread");
        assert_eq!(outcome, ProbeOutcome::UnicodeSafe);
    }

    #[test]
    fn probe_asks_about_every_codepoint_and_cleans_up_after_itself() {
        let n = inventory_codepoints().len();
        let mut out = Vec::new();
        let mut inp = Fake::always(2, n);
        probe(&mut out, &mut inp);

        let written = String::from_utf8(out).expect("probe wrote valid utf8");
        assert_eq!(
            written.matches("\x1b[6n").count(),
            n,
            "every inventory codepoint must be asked about, not a sample"
        );
        for g in inventory_codepoints() {
            assert!(written.contains(g), "codepoint {g} was never written");
        }
        assert!(
            written.ends_with("\r\x1b[2K"),
            "the probe must erase the line it drew on"
        );
    }
}
