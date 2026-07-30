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

use libghostty_vt::terminal::{Options, Terminal};

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
        let term = Terminal::new(Options {
            cols,
            rows,
            max_scrollback: DEFAULT_SCROLLBACK,
        })
        .ok()?;
        Some(Self { term })
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
