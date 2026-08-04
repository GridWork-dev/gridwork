//! The rows-that-changed view of a grid: what a drawing surface needs to
//! repaint, and nothing else — glyphs and [the style](crate::style) they
//! carry, never a resolved pixel.
//!
//! This is the *other* meaning of "delta" — see [`crate::record`] for the one
//! that gets persisted and replayed. A frame here is stamped with a sequence
//! number issued by the recording, so a renderer and a reattaching client are
//! always talking about the same point in the same stream.

use libghostty_vt::render::{CellIterator, Dirty, RenderState, RowIterator};
use libghostty_vt::screen::CellWide;

use crate::Grid;
use crate::style::CellStyle;

/// One row of a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// Zero-based row index within the viewport.
    pub y: u16,
    pub text: String,
    /// Per-cell style, one entry per grapheme pushed into `text`, in the same
    /// order — NOT one entry per column. A spacer cell contributes neither a
    /// grapheme nor a style entry, the same rule `text` already follows for
    /// wide characters (see the loop below), so the two stay index-aligned.
    ///
    // ponytail: a flat `Vec`, not run-length-encoded by shared style. Most of
    // a row shares one style, so an RLE span list would usually be smaller;
    // add it if a profiled frame size ever calls for it — nothing here has
    // measured one yet.
    pub styles: Vec<CellStyle>,
}

/// What a surface has to redraw to be current with the grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    /// The recording sequence this frame reflects, as handed to
    /// [`Renderer::frame`].
    pub seq: u64,
    pub cols: u16,
    pub rows: u16,
    /// Cursor as zero-based `(column, row)`, absent when it is hidden.
    pub cursor: Option<(u16, u16)>,
    /// When set, `changed` holds every row and the surface must repaint all of
    /// it. libghostty-vt raises this for changes no per-row flag can express —
    /// a resize being the obvious one, where every row moved even if no row's
    /// contents did.
    pub full: bool,
    /// Rows that changed, ascending. Empty means nothing to draw.
    pub changed: Vec<Row>,
}

/// Turns a [`Grid`] into successive [`Frame`]s.
///
/// One renderer per grid, and no more: libghostty-vt clears the terminal's
/// dirty flags as a side effect of producing a snapshot, so a second renderer
/// on the same grid would see a clean screen and draw nothing. That is why
/// there is no `Grid::frame` convenience — it would make the mistake easy.
pub struct Renderer {
    state: RenderState<'static>,
    rows: RowIterator<'static>,
    cells: CellIterator<'static>,
}

impl Renderer {
    /// Returns `None` if libghostty-vt cannot allocate its render state.
    pub fn new() -> Option<Self> {
        Some(Self {
            state: RenderState::new().ok()?,
            rows: RowIterator::new().ok()?,
            cells: CellIterator::new().ok()?,
        })
    }

    /// The changes since the last call, stamped with `seq`.
    ///
    /// `seq` is the recording sequence the grid has been fed up to — see
    /// [`crate::record::Recording::record`]. It is passed in rather than
    /// counted here so that a frame and a reattach request cannot end up
    /// numbering the same session two different ways.
    ///
    /// Calling this consumes the grid's dirty state: two calls in a row report
    /// the second as clean.
    pub fn frame(&mut self, grid: &Grid, seq: u64) -> Option<Frame> {
        let snapshot = self.state.update(grid.terminal()).ok()?;
        let dirty = snapshot.dirty().ok()?;
        let cols = snapshot.cols().ok()?;
        let rows = snapshot.rows().ok()?;
        let cursor = snapshot
            .cursor_visible()
            .ok()?
            .then(|| {
                snapshot
                    .cursor_viewport()
                    .ok()
                    .flatten()
                    .map(|c| (c.x, c.y))
            })
            .flatten();

        let full = dirty == Dirty::Full;
        let mut changed = Vec::new();
        let mut cluster = String::new();

        if dirty != Dirty::Clean {
            let mut row_iter = self.rows.update(&snapshot).ok()?;
            let mut y = 0u16;
            while let Some(row) = row_iter.next() {
                let row_dirty = row.dirty().ok()?;
                if full || row_dirty {
                    let mut text = String::new();
                    let mut styles = Vec::new();
                    let mut cell_iter = self.cells.update(row).ok()?;
                    while let Some(cell) = cell_iter.next() {
                        // Derivation: UAX-11 — East Asian Wide (W) and
                        // Fullwidth (F) characters take one Em in a fixed-pitch
                        // font against half an Em for the narrow classes, i.e.
                        // twice the advance. The annex stops there, and says so
                        // itself: the property is not intended for use by
                        // modern terminal emulators without tailoring. So "two
                        // columns" is this crate reading that ratio into a cell
                        // grid, NOT a sentence UAX-11 contains — cite it for
                        // the ratio, not for the conclusion.
                        //
                        // What the shape below needs is only that some
                        // characters take two cells: libghostty-vt models one
                        // as the character followed by a spacer cell marked
                        // do-not-render, so the spacer contributes nothing here
                        // and a row's char count is NOT its column count. Width
                        // stays the consumer's to compute from the text rather
                        // than something faked here by padding.
                        if matches!(
                            cell.raw_cell().ok()?.wide().ok()?,
                            CellWide::SpacerTail | CellWide::SpacerHead
                        ) {
                            continue;
                        }
                        // Each cell goes into a scratch buffer and is appended
                        // here, rather than passing `text` down directly.
                        //
                        // The library's call reads like an append — it takes
                        // the string you are building, and its safety note is
                        // about preserving that string's existing contents —
                        // but as of libghostty-vt 0.2.1 it REPLACES. Running
                        // one string through the cells of a row holding "abc"
                        // leaves it containing "c". Clearing the scratch buffer
                        // each time is correct under either behavior, so an
                        // upstream fix cannot silently start doubling cells.
                        cluster.clear();
                        cell.graphemes_utf8(&mut cluster).ok()?;
                        if cluster.is_empty() {
                            // An empty cell is a space, not nothing. Dropping
                            // it would slide every later cell left and hand a
                            // renderer a row whose columns no longer line up
                            // with the grid's.
                            text.push(' ');
                        } else {
                            text.push_str(&cluster);
                        }
                        // Pushed for the same cell `text` just gained a
                        // grapheme for, in the same iteration — that is what
                        // keeps `styles[i]` describing `text`'s i-th grapheme
                        // rather than some other cell's.
                        styles.push(CellStyle::from(cell.style().ok()?));
                    }
                    changed.push(Row { y, text, styles });
                }
                // Marking the row clean is the caller's job, not a side effect
                // of reading it. Skipping this leaves every row dirty forever,
                // so every frame reports the whole screen as changed and the
                // delta degrades into a full repaint that still *looks* like a
                // delta.
                if row_dirty {
                    row.set_dirty(false).ok()?;
                }
                y += 1;
            }
        }
        // Same for the frame-level flag, which is why it is read into `dirty`
        // above before anything clears it.
        snapshot.set_dirty(Dirty::Clean).ok()?;

        Some(Frame {
            seq,
            cols,
            rows,
            cursor,
            full,
            changed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row's text with the blank tail removed, so assertions read against
    /// what was written rather than the column count.
    fn trimmed(frame: &Frame, y: u16) -> String {
        frame
            .changed
            .iter()
            .find(|r| r.y == y)
            .map(|r| r.text.trim_end().to_owned())
            .unwrap_or_default()
    }

    #[test]
    fn the_first_frame_carries_the_screen_and_the_next_carries_nothing() {
        let mut grid = Grid::new(20, 4).expect("20x4");
        let mut renderer = Renderer::new().expect("render state");

        grid.write(b"one\r\ntwo");
        let first = renderer.frame(&grid, 7).expect("frame");
        assert_eq!(first.seq, 7, "the frame carries the sequence it was given");
        assert_eq!((first.cols, first.rows), (20, 4));
        assert_eq!(trimmed(&first, 0), "one");
        assert_eq!(trimmed(&first, 1), "two");
        assert_eq!(
            first.changed[0].text.chars().count(),
            20,
            "an all-narrow row is one char per column, blanks included"
        );

        // Nothing has been written since, so there is nothing to repaint. This
        // is the whole point of a delta, and the thing that breaks silently if
        // something else consumed the dirty state first.
        let second = renderer.frame(&grid, 8).expect("frame");
        assert!(
            second.changed.is_empty() && !second.full,
            "an unchanged grid should produce no rows: {second:?}"
        );
    }

    #[test]
    fn untouched_rows_stay_out_of_the_frame() {
        let mut grid = Grid::new(20, 4).expect("20x4");
        let mut renderer = Renderer::new().expect("render state");
        grid.write(b"one\r\ntwo\r\nthree");
        renderer.frame(&grid, 0).expect("drain the initial paint");

        // Derivation: ECMA-48 §8.3.21 — CUP takes a 1-based line then column,
        // so row index 1 is line 2.
        grid.write(b"\x1b[2;1HTWO");
        let frame = renderer.frame(&grid, 1).expect("frame");

        assert!(!frame.full, "a cell edit is not a full repaint");
        // Row 1 changed its text. Row 2 changed too, and it is worth being
        // precise about why: the cursor was sitting there and has now left, so
        // the surface has to repaint that row to erase the caret. Rows 0 and 3
        // were not involved either way, and their absence is what makes this a
        // delta rather than a screen.
        assert_eq!(
            frame.changed.iter().map(|r| r.y).collect::<Vec<_>>(),
            vec![1, 2],
            "expected the edited row and the row the cursor left: {:?}",
            frame.changed
        );
        assert_eq!(trimmed(&frame, 1), "TWO");
        assert_eq!(frame.cursor, Some((3, 1)), "cursor sits after the edit");
    }

    #[test]
    fn a_resize_is_a_full_repaint() {
        let mut grid = Grid::new(20, 4).expect("20x4");
        let mut renderer = Renderer::new().expect("render state");
        grid.write(b"hello");
        renderer.frame(&grid, 0).expect("drain the initial paint");

        grid.resize(30, 6).expect("30x6 is valid");
        let frame = renderer.frame(&grid, 1).expect("frame");

        assert!(frame.full, "every row moved, so every row must be redrawn");
        assert_eq!((frame.cols, frame.rows), (30, 6));
        assert_eq!(
            frame.changed.len(),
            6,
            "a full repaint carries every row, not just the dirty ones"
        );
        assert_eq!(trimmed(&frame, 0), "hello");
    }

    #[test]
    fn a_multi_codepoint_grapheme_stays_in_one_cell() {
        // The cluster-accumulation bug this guards is invisible with ASCII:
        // one wrong buffer still yields one character per cell. An emoji whose
        // cluster is several codepoints only survives if the whole cluster is
        // taken and appended.
        //
        // Derivation: UAX-29 — a grapheme cluster is what a reader perceives as
        // one character, and ZWJ sequences join several codepoints into one.
        let mut grid = Grid::new(10, 1).expect("10x1");
        let mut renderer = Renderer::new().expect("render state");
        grid.write("a\u{1F469}\u{200D}\u{1F4BB}b".as_bytes());

        let frame = renderer.frame(&grid, 0).expect("frame");
        let text = trimmed(&frame, 0);
        assert!(
            text.starts_with('a') && text.ends_with('b'),
            "the cells either side of the cluster should survive it: {text:?}"
        );
        assert!(
            text.contains('\u{200D}'),
            "the joiner should still be there, not truncated to the base: {text:?}"
        );
    }

    #[test]
    fn a_gap_between_words_keeps_its_columns() {
        // Dropping empty cells instead of emitting a space would turn this
        // into "ab" and slide every later column left. It is invisible in a
        // test that only writes contiguous text, which is why this one does
        // not.
        let mut grid = Grid::new(10, 1).expect("10x1");
        let mut renderer = Renderer::new().expect("render state");
        // Derivation: ECMA-48 §8.3.9 — CHA moves the active position to the
        // n-th character position on the current line, 1-based, so `\x1b[5G`
        // lands on column index 4 and leaves columns 1..4 untouched. That gap
        // of never-written cells is the case under test; writing spaces into
        // it would exercise a different thing entirely.
        grid.write(b"a\x1b[5Gb");

        let frame = renderer.frame(&grid, 0).expect("frame");
        assert_eq!(trimmed(&frame, 0), "a   b");
    }
}
