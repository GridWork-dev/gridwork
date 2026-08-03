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

use gwk_theme::probe::{CellReport, ProbeOutcome, evaluate, inventory_codepoints};

/// How many bytes we will read chasing a single reply before giving up.
///
/// A terminal that answers at all answers within a few bytes. This bound is what
/// stops a terminal that never answers from hanging the console at startup —
/// the caller gets `Inconclusive` and degrades, which is the designed outcome
/// rather than an error path.
const REPLY_BUDGET: usize = 32;

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
