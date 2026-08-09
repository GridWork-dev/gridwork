//! Input plumbing: the session bracket, click hit-testing, and the copy path.
//!
//! Mouse capture is **on, and crossterm-mediated** — [`EnableMouseCapture`]
//! plus the typed [`MouseEvent`], never hand-rolled SGR decoding. That wording
//! is load-bearing: console code that hand-implements terminal-protocol
//! handling is clean-room-gated, and library-mediated I/O is the recorded
//! exemption this module rides. The same boundary picks the clipboard
//! mechanism below.
//!
//! # The mouse is an accelerator, never the only path
//!
//! Every target a lens registers in a [`HitMap`] must also be reachable by
//! keyboard — a pattern that only works as direct manipulation violates the
//! CLI-twin rule. The map answers "what did this click land on"; it does not
//! excuse a lens from answering the same question for a keypress.
//!
//! # Copy over SSH
//!
//! Three paths, in the ruled order:
//!
//! 1. **OSC 52, primary** ([`copy_id`]) — the terminal's own clipboard write,
//!    which works from the far end of an SSH session because the escape
//!    travels the same wire the frames do. Emitted through crossterm's
//!    [`CopyToClipboard`] command, for the exemption reason above.
//! 2. **The ID itself, fallback** — every copyable value is a
//!    [`RetypableId`], short enough to read off one screen and type into
//!    another. When a terminal or multiplexer drops OSC 52, the fallback is
//!    already on screen.
//! 3. **Shift-drag native selection, degraded** — capture-on does not remove
//!    it (the modifier bypasses mouse reporting in xterm, VTE, iTerm2 and
//!    most descendants), but it is terminal-dependent and unreliable through
//!    nested tmux, which disqualifies it as a documented answer. It is listed
//!    here so nobody promotes it later without meeting this sentence.
//!
//! The copy answer is deliberately capture-independent: the next phase's
//! multiplexer will want the mouse, and a copy path that only worked with
//! capture off would not survive it.
//!
//! # Why only IDs are copyable
//!
//! The fallback path PRINTS the value, and the threat model strips escapes
//! from anything echoed outside the virtual-terminal paint. [`RetypableId`]
//! refuses control bytes, whitespace, and anything non-ASCII at construction,
//! so the value that reaches the clipboard — or the screen — cannot carry a
//! sequence. What the type enforces is the SHAPE — short, printable, sequence
//! free; handing it kernel-minted IDs rather than fragments of session output
//! is the caller's obligation, and the shape rules make almost any real
//! session fragment fail construction anyway.

use std::fmt;
use std::io::{self, Write};
use std::time::Duration;

use crossterm::QueueableCommand;
use crossterm::clipboard::CopyToClipboard;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::layout::{Position, Rect};

/// The ruled default frame cadence: 33ms, not 16.
///
/// Measured-good over SSH where 16ms is not, and the base rung of the adaptive
/// ladder (16 → 33 → 66 → 125 → off) the production loop wires to `--motion`.
/// Lenses take this constant rather than re-deciding the number.
pub const TICK: Duration = Duration::from_millis(33);

/// The longest string a human can be asked to retype from another screen.
///
/// A UUID is 36 characters; this leaves room for a typed prefix on top of one
/// and no room for a paragraph. The bound is what makes the fallback path
/// honest — "copy failed, type it" only works for values short enough that
/// the sentence is not a joke.
pub const RETYPE_BUDGET: usize = 64;

/// Enter the console's terminal posture: alternate screen, then mouse capture.
///
/// Everything here is a queued crossterm command, which is why the function is
/// generic over the writer and therefore testable. What it deliberately does
/// NOT do is enable raw mode — that is a tty ioctl, not a byte stream, and it
/// belongs to the binary that owns the real terminal (the same split
/// [`probe`](crate::probe) documents).
pub fn enter<W: Write>(out: &mut W) -> io::Result<()> {
    out.queue(EnterAlternateScreen)?;
    out.queue(EnableMouseCapture)?;
    out.flush()
}

/// Leave the console's terminal posture, unwinding [`enter`] in reverse.
///
/// Mouse capture off first, alternate screen last: modes are a stack, and a
/// terminal left with capture on after the screen is restored eats the
/// user's next click at their own shell.
///
/// The binary that calls [`enter`] owes this call on EVERY exit path,
/// panics included — ratatui's own restore hook does not cover mouse
/// capture, because ratatui never enables it. Wire it into the panic hook
/// alongside the raw-mode restore.
pub fn exit<W: Write>(out: &mut W) -> io::Result<()> {
    out.queue(DisableMouseCapture)?;
    out.queue(LeaveAlternateScreen)?;
    out.flush()
}

/// What this frame's clicks can land on.
///
/// A lens registers a region for each row (or glyph pair — the two-cell Hall
/// target with its one-cell floor is just a narrow `Rect`) as it draws, and
/// asks the map where a mouse event landed. The map is frame-scoped: clear it
/// when layout changes, or re-register on every draw, because a stale region
/// is a click delivered to a row that is no longer there.
#[derive(Debug)]
pub struct HitMap<T> {
    regions: Vec<(Rect, T)>,
}

// Written out rather than derived: the derive would demand `T: Default` for a
// map whose empty state needs nothing from `T`, and the lens target types have
// no reason to have defaults.
impl<T> Default for HitMap<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> HitMap<T> {
    pub fn new() -> Self {
        HitMap {
            regions: Vec::new(),
        }
    }

    /// Forget every region. Call when the frame's layout no longer holds.
    pub fn clear(&mut self) {
        self.regions.clear();
    }

    /// Claim a region for a target. Later registrations win overlaps, because
    /// later draws paint on top.
    pub fn register(&mut self, area: Rect, target: T) {
        self.regions.push((area, target));
    }

    /// The registered targets in paint order — the default keyboard walk for
    /// a lens whose whole target set fits the frame. A windowed lens exposes a
    /// separate full target order (the Board does) and uses this map only for
    /// visible click regions. In both cases the mouse remains an accelerator,
    /// never the only route to a target.
    pub fn targets(&self) -> impl Iterator<Item = &T> {
        self.regions.iter().map(|(_, target)| target)
    }

    /// The target under a cell, if any.
    pub fn hit(&self, column: u16, row: u16) -> Option<&T> {
        let position = Position::new(column, row);
        self.regions
            .iter()
            .rev()
            .find(|(area, _)| area.contains(position))
            .map(|(_, target)| target)
    }

    /// The target a click selects: a **left-button press**, nothing else.
    ///
    /// Press rather than release is the terminal idiom, and every other kind —
    /// drags, releases, motion, scroll — is deliberately not a selection here.
    /// Scroll routing is a lens decision; conflating it with clicking would
    /// select whatever the wheel happened to pass over.
    pub fn click(&self, event: &MouseEvent) -> Option<&T> {
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => self.hit(event.column, event.row),
            _ => None,
        }
    }
}

/// A value short and clean enough to be retyped by hand.
///
/// The construction rules ARE the security posture (see the module doc): only
/// ASCII graphic characters, no whitespace, nothing that could smuggle a
/// control sequence onto the screen the fallback prints to, and no more than
/// [`RETYPE_BUDGET`] of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetypableId(String);

impl RetypableId {
    pub fn new(id: &str) -> Result<Self, IdError> {
        if id.is_empty() {
            return Err(IdError::Empty);
        }
        // Character check before length check: everything that survives it is
        // pure ASCII, so the length below counts characters and bytes at once
        // — in the other order a multi-byte character would be reported as a
        // length problem measured in a unit the message does not name.
        if let Some(c) = id.chars().find(|c| !c.is_ascii_graphic()) {
            return Err(IdError::Unretypable(c));
        }
        if id.len() > RETYPE_BUDGET {
            return Err(IdError::TooLong(id.len()));
        }
        Ok(RetypableId(id.to_owned()))
    }

    /// The fallback path: the ID itself, for reading off the screen.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RetypableId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why a string cannot be a [`RetypableId`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdError {
    Empty,
    /// Longer than [`RETYPE_BUDGET`]; carries the offending length.
    TooLong(usize),
    /// Contains a character a hand cannot reliably retype — whitespace, a
    /// control byte, or anything outside ASCII graphic. Carries the first
    /// offender.
    Unretypable(char),
}

impl fmt::Display for IdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IdError::Empty => write!(f, "an empty string is not an ID"),
            IdError::TooLong(len) => write!(
                f,
                "{len} characters cannot be retyped from another screen (budget {RETYPE_BUDGET})"
            ),
            IdError::Unretypable(c) => write!(f, "{c:?} cannot be reliably retyped"),
        }
    }
}

impl std::error::Error for IdError {}

/// Copy an ID to the clipboard over OSC 52 — the primary copy path.
///
/// Queued through crossterm's [`CopyToClipboard`], so the sequence on the wire
/// is the library's, not ours. A terminal that ignores OSC 52 ignores it
/// silently; that is why the value's own display IS the fallback rather than
/// this function returning a verdict it cannot know.
pub fn copy_id<W: Write>(out: &mut W, id: &RetypableId) -> io::Result<()> {
    out.queue(CopyToClipboard::to_clipboard_from(id.as_str()))?;
    out.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes_of<F: FnOnce(&mut Vec<u8>) -> io::Result<()>>(f: F) -> String {
        let mut out = Vec::new();
        f(&mut out).expect("writing to a Vec cannot fail");
        String::from_utf8(out).expect("crossterm emits valid utf8")
    }

    #[test]
    fn input_the_session_bracket_sets_and_clears_the_same_modes() {
        let entered = bytes_of(enter);
        let exited = bytes_of(exit);

        // Alternate screen plus the five mouse-reporting modes EnableMouseCapture
        // sets. Every mode the bracket sets it must also clear — an unmatched
        // pair is a terminal handed back broken.
        for mode in ["?1049", "?1000", "?1002", "?1003", "?1015", "?1006"] {
            assert!(
                entered.contains(&format!("{mode}h")),
                "enter must set {mode}"
            );
            assert!(
                exited.contains(&format!("{mode}l")),
                "exit must clear {mode}"
            );
        }
    }

    #[test]
    fn input_the_bracket_unwinds_in_reverse_order() {
        let entered = bytes_of(enter);
        let exited = bytes_of(exit);

        // Enter: screen first, capture second. Exit: capture first, screen
        // last. Modes are a stack; crossing the pairs leaves capture aimed at
        // the caller's shell for a moment on the way out.
        assert!(
            entered.find("?1049h").expect("alt screen") < entered.find("?1000h").expect("capture")
        );
        assert!(
            exited.find("?1000l").expect("capture") < exited.find("?1049l").expect("alt screen")
        );
    }

    #[test]
    fn input_a_click_inside_a_row_selects_it() {
        let mut map = HitMap::new();
        map.register(Rect::new(2, 5, 10, 1), "row-a");
        assert_eq!(map.hit(2, 5), Some(&"row-a"), "the left edge is inside");
        assert_eq!(map.hit(11, 5), Some(&"row-a"), "the last cell is inside");
    }

    #[test]
    fn input_a_click_one_cell_off_a_row_must_not_select_it() {
        // The seeded miss: every neighbouring cell of a 10-wide row at (2,5).
        let mut map = HitMap::new();
        map.register(Rect::new(2, 5, 10, 1), "row-a");
        assert_eq!(map.hit(1, 5), None, "one cell left");
        assert_eq!(map.hit(12, 5), None, "one cell past the right edge");
        assert_eq!(map.hit(5, 4), None, "one row above");
        assert_eq!(map.hit(5, 6), None, "one row below");
    }

    #[test]
    fn input_overlapping_regions_resolve_to_the_last_registered() {
        let mut map = HitMap::new();
        map.register(Rect::new(0, 0, 20, 3), "under");
        map.register(Rect::new(5, 1, 5, 1), "over");
        assert_eq!(
            map.hit(6, 1),
            Some(&"over"),
            "later draws paint on top, so later registrations win"
        );
        assert_eq!(map.hit(0, 0), Some(&"under"));
    }

    #[test]
    fn input_a_two_cell_pair_and_a_one_cell_floor_are_both_hittable() {
        // The ratified hit-target geometry: the two-cell glyph pair is the
        // target and one cell is the floor. Both must be real targets, or the
        // densest rung has glyphs a mouse cannot reach.
        let mut map = HitMap::new();
        map.register(Rect::new(4, 2, 2, 1), "pair");
        map.register(Rect::new(9, 2, 1, 1), "floor");
        assert_eq!(map.hit(4, 2), Some(&"pair"));
        assert_eq!(map.hit(5, 2), Some(&"pair"));
        assert_eq!(map.hit(9, 2), Some(&"floor"));
        assert_eq!(map.hit(10, 2), None, "the floor is one cell, not two");
    }

    #[test]
    fn input_only_a_left_button_press_is_a_click() {
        use crossterm::event::KeyModifiers;

        let mut map = HitMap::new();
        map.register(Rect::new(0, 0, 10, 1), "row");
        let at = |kind| MouseEvent {
            kind,
            column: 3,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };

        assert_eq!(
            map.click(&at(MouseEventKind::Down(MouseButton::Left))),
            Some(&"row")
        );
        for kind in [
            MouseEventKind::Down(MouseButton::Right),
            MouseEventKind::Down(MouseButton::Middle),
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Moved,
            MouseEventKind::ScrollUp,
            MouseEventKind::ScrollDown,
        ] {
            assert_eq!(
                map.click(&at(kind)),
                None,
                "{kind:?} is not a selection gesture"
            );
        }
    }

    #[test]
    fn input_clear_forgets_the_frame() {
        let mut map = HitMap::new();
        map.register(Rect::new(0, 0, 10, 1), "row");
        map.clear();
        assert_eq!(
            map.hit(3, 0),
            None,
            "a cleared map must not deliver clicks to rows that are gone"
        );
    }

    #[test]
    fn input_copy_rides_osc52_with_the_id_encoded() {
        let id = RetypableId::new("t-42").expect("a plain short ID");
        let written = bytes_of(|out| copy_id(out, &id));
        // The whole sequence, exactly: OSC 52, clipboard destination `c`,
        // base64 of the ID, ST terminator. Asserting the full string means a
        // crossterm that changed its emission shows up here, not in a user's
        // clipboard.
        assert_eq!(written, "\x1b]52;c;dC00Mg==\x1b\\");
    }

    #[test]
    fn input_an_id_that_cannot_be_retyped_is_refused() {
        assert_eq!(RetypableId::new(""), Err(IdError::Empty));
        assert_eq!(
            RetypableId::new(&"x".repeat(RETYPE_BUDGET + 1)),
            Err(IdError::TooLong(RETYPE_BUDGET + 1))
        );
        // Whitespace, a control byte, an escape, and non-ASCII: each is a
        // value the fallback path would put on screen, which is exactly where
        // it must never arrive.
        assert_eq!(RetypableId::new("a b"), Err(IdError::Unretypable(' ')));
        assert_eq!(
            RetypableId::new("a\x07b"),
            Err(IdError::Unretypable('\x07'))
        );
        assert_eq!(
            RetypableId::new("a\x1b[31mred"),
            Err(IdError::Unretypable('\x1b'))
        );
        assert_eq!(RetypableId::new("café"), Err(IdError::Unretypable('é')));
    }

    #[test]
    fn input_the_fallback_is_the_id_itself() {
        let id = RetypableId::new("task-0042").expect("a plain short ID");
        assert_eq!(id.as_str(), "task-0042");
        assert_eq!(id.to_string(), "task-0042", "display IS the fallback path");
    }

    #[test]
    fn input_a_budget_length_id_is_accepted() {
        // The boundary itself: exactly RETYPE_BUDGET characters passes, so the
        // TooLong test above is proven to sit one past a real edge.
        let at_budget = "x".repeat(RETYPE_BUDGET);
        assert!(RetypableId::new(&at_budget).is_ok());
    }
}
