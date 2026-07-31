//! PTY engine — the authoritative terminal grid.
//!
//! This crate owns "terminal truth": bytes written by a child process are fed to
//! a VT parser, and the resulting grid is what every GridWork surface renders
//! from. Nothing here scrapes a screen or infers state from output shape.
//!
//! # What this crate is, and is not
//!
//! The VT implementation is [libghostty-vt], consumed through its **safe** Rust
//! wrapper rather than the raw `-sys` bindings. That is a deliberate choice with
//! a consequence worth stating plainly: this crate contains no `unsafe` of its
//! own, so the FFI boundary is not ours to audit — it is a dependency we accept.
//! libghostty-vt is pre-1.0 and says so; see `pins.env` for why all three pins
//! move together.
//!
//! # Building
//!
//! Requires Zig and a ghostty source tree, both pinned in `pins.env`. The build
//! reaches the network zero times when `GHOSTTY_SOURCE_DIR` and
//! `GHOSTTY_ZIG_SYSTEM_DIR` are both set — `GHOSTTY_SOURCE_DIR` alone is not
//! enough, because Zig still resolves the package graph over the network.
//!
//! ```text
//! ./tools/pty-toolchain.sh                     # fetch the pinned tree + packages
//! eval "$(./tools/pty-toolchain.sh --env)"     # export the two variables
//! cargo test -p gwk-pty
//! ```
//!
//! CI runs the same script in the `pty` job. This crate is not a default
//! workspace member, so `cargo test` at the root does not need any of it.
//!
//! [libghostty-vt]: https://crates.io/crates/libghostty-vt

use libghostty_vt::fmt::{Format, Formatter, FormatterOptions};
use libghostty_vt::terminal::{Options, Terminal};

pub mod attach;
pub mod record;
pub mod render;
pub mod session;

pub use attach::{Attach, CaughtUp};
pub use record::{Entry, Event, Recording};
pub use render::{Frame, Renderer};
pub use session::{Session, SpawnError};

/// How much scrollback a grid retains before evicting the oldest rows.
///
/// ponytail: one constant, not a knob. The real retention policy belongs with
/// `pty_recording` once there is a recording to measure; picking a tuned number
/// before then would be a guess wearing a config option.
pub const DEFAULT_SCROLLBACK: usize = 10_000;

/// An authoritative terminal grid: bytes in, screen state out.
pub struct Grid {
    term: Terminal<'static, 'static>,
}

impl Grid {
    /// Create a grid of `cols` × `rows` cells.
    ///
    /// Returns `None` if libghostty-vt rejects the dimensions, which it does for
    /// a zero in either axis.
    pub fn new(cols: u16, rows: u16) -> Option<Self> {
        Self::with_scrollback(cols, rows, DEFAULT_SCROLLBACK)
    }

    /// A grid retaining `max_scrollback` lines of history rather than the
    /// default.
    ///
    /// The knob exists because eviction is a behavior worth testing and
    /// filling ten thousand lines to reach it is not a test, it is a wait.
    pub fn with_scrollback(cols: u16, rows: u16, max_scrollback: usize) -> Option<Self> {
        let term = Terminal::new(Options {
            cols,
            rows,
            max_scrollback,
        })
        .ok()?;
        Some(Self { term })
    }

    /// Lines of history above the viewport.
    pub fn scrollback_rows(&self) -> Option<usize> {
        self.term.scrollback_rows().ok()
    }

    /// Feed child-process output to the parser.
    ///
    /// Takes an arbitrary byte slice, not a `str`: a child emits whatever it
    /// likes, including invalid UTF-8 and partial sequences split across reads,
    /// and the parser is what resolves that — not the caller.
    pub fn write(&mut self, bytes: &[u8]) {
        self.term.vt_write(bytes);
    }

    /// Cursor position as a zero-based `(column, row)`.
    pub fn cursor(&self) -> Option<(u16, u16)> {
        Some((self.term.cursor_x().ok()?, self.term.cursor_y().ok()?))
    }

    /// Grid size as `(cols, rows)`.
    pub fn size(&self) -> Option<(u16, u16)> {
        Some((self.term.cols().ok()?, self.term.rows().ok()?))
    }

    /// Change the grid's dimensions, reflowing its contents.
    ///
    /// Returns `None` for a zero in either axis, matching [`Grid::new`].
    ///
    /// The two pixel arguments libghostty-vt takes are passed as zero on
    /// purpose. They feed image protocols and pixel-unit size reports, and a
    /// grid with no renderer attached has no cell size to report — inventing
    /// one would put a number a child might act on into the terminal's state.
    /// A grid starts at zero pixels too, so this keeps resize consistent with
    /// construction rather than introducing a value halfway through.
    pub fn resize(&mut self, cols: u16, rows: u16) -> Option<()> {
        if cols == 0 || rows == 0 {
            return None;
        }
        self.term.resize(cols, rows, 0, 0).ok()
    }

    /// The active screen as plain text, one line per row.
    ///
    /// This is the crate's canonical rendering: it is what golden frames are
    /// captured as, and what two grids are compared through when a reattached
    /// session has to prove it landed on the same screen. Whitespace is NOT
    /// trimmed — a row of spaces and an empty row are different screen states,
    /// and a comparison that could not tell them apart would pass on a real
    /// divergence.
    pub fn text(&self) -> Option<String> {
        let mut formatter = Formatter::new(
            &self.term,
            FormatterOptions::new().with_format(Format::Plain),
        )
        .ok()?;
        let mut buf = vec![0u8; formatter.format_len().ok()?];
        let len = formatter.format_buf(&mut buf).ok()?;
        buf.truncate(len);
        String::from_utf8(buf).ok()
    }

    /// One row of [`text`](Self::text), zero-based, or `None` past the end.
    pub fn row_text(&self, y: u16) -> Option<String> {
        self.text()?.lines().nth(usize::from(y)).map(str::to_owned)
    }

    /// The screen as VT sequences that reproduce it when written to a fresh
    /// grid of the same size.
    ///
    /// This is what a client attaching mid-session is sent. Replaying the whole
    /// recording would work too, but only while the recording still reaches
    /// back that far — after eviction it does not, and a snapshot has no such
    /// horizon.
    ///
    /// Derivation: ECMA-48 §8.3.21 — CUP sets the active position to a line and
    /// column given as 1-based parameters. The trailing CUP is not decoration:
    /// libghostty-vt's VT dump emits screen *contents*, which leaves the cursor
    /// wherever the last cell put it, so without this a reattached client draws
    /// the right screen with the caret in the wrong place.
    pub fn snapshot_vt(&self) -> Option<Vec<u8>> {
        let mut formatter =
            Formatter::new(&self.term, FormatterOptions::new().with_format(Format::Vt)).ok()?;
        let mut buf = vec![0u8; formatter.format_len().ok()?];
        let len = formatter.format_buf(&mut buf).ok()?;
        buf.truncate(len);

        let (x, y) = self.cursor()?;
        buf.extend_from_slice(format!("\x1b[{};{}H", y + 1, x + 1).as_bytes());
        Some(buf)
    }

    /// The parser, for the render path inside this crate.
    pub(crate) fn terminal(&self) -> &Terminal<'static, 'static> {
        &self.term
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_forward_advances_by_the_parameter() {
        let mut grid = Grid::new(80, 24).expect("80x24 is a valid grid");

        // Derivation: ECMA-48 §8.3.20 — CUF moves the active position forward by
        // Pn columns, with parameter default Pn = 1. Both halves are asserted
        // below; the default is the half a hand-written parser gets wrong.
        grid.write(b"hello\x1b[5C world");
        assert_eq!(grid.cursor(), Some((16, 0)), "5 + 5 + 6");

        let mut grid = Grid::new(80, 24).expect("80x24 is a valid grid");
        grid.write(b"a\x1b[Cb");
        assert_eq!(
            grid.cursor(),
            Some((3, 0)),
            "omitted parameter defaults to 1"
        );
    }

    #[test]
    fn a_zero_dimension_is_refused_rather_than_clamped() {
        // A silently clamped 0 would produce a grid whose size does not match
        // what the caller asked for, and every downstream delta would be
        // computed against the wrong geometry.
        assert!(Grid::new(0, 24).is_none());
        assert!(Grid::new(80, 0).is_none());
    }
}
