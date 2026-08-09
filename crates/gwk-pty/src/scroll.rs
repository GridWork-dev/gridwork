//! Scrollback navigation over a [`Grid`]'s retained history.
//!
//! A [`HistoryView`] is one viewer's scroll position, not the terminal's:
//! the grid keeps parsing child output and the live screen keeps moving
//! while a view is scrolled back. The position is a single offset counted
//! from the live edge, so "how far back am I" keeps meaning the same thing
//! while new rows arrive — and eviction shrinking history under a held view
//! clamps the offset instead of erroring, because the rows the view was
//! holding no longer exist anywhere and a refusal would pin the caller to
//! a position that cannot be read.
//!
//! Derivation: none — original scrollback-navigation design over this
//! crate's own grid API; the offset-from-live model and its clamping are
//! this crate's choices and assert no external behavior.

use crate::Grid;

/// One viewer's position in a grid's scrollback.
///
/// The offset is how many rows above the live viewport the view sits;
/// zero is live. Counted from the live edge rather than from the top so
/// that new output and eviction both leave a held position meaning "this
/// many rows before now" — an absolute row number would silently name a
/// different line every time either happened.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct HistoryView {
    offset: usize,
}

impl HistoryView {
    /// A view at the live edge.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the view is at the live edge (not scrolled back).
    pub fn is_live(&self) -> bool {
        self.offset == 0
    }

    /// Rows above the live viewport the view currently sits.
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Scroll further into history, stopping at the oldest retained row.
    pub fn scroll_up(&mut self, lines: usize, grid: &Grid) {
        let held = grid.scrollback_rows().unwrap_or(0);
        self.offset = self.offset.saturating_add(lines).min(held);
    }

    /// Scroll back toward the live edge.
    pub fn scroll_down(&mut self, lines: usize) {
        self.offset = self.offset.saturating_sub(lines);
    }

    /// Jump to the oldest retained row.
    pub fn to_top(&mut self, grid: &Grid) {
        self.offset = grid.scrollback_rows().unwrap_or(0);
    }

    /// Jump back to the live edge.
    pub fn to_live(&mut self) {
        self.offset = 0;
    }

    /// The viewport-sized window of text at this position.
    ///
    /// Clamps the offset first: a reflow may have shrunk history since the
    /// last call, and the clamped read is the honest answer — the rows the
    /// view was holding are gone, so the oldest retained row is now the
    /// deepest position that exists. At offset zero the window is the live
    /// viewport — not [`Grid::text`], which renders every retained row.
    pub fn text(&mut self, grid: &Grid) -> Option<String> {
        let held = grid.scrollback_rows()?;
        self.offset = self.offset.min(held);
        let (_, rows) = grid.size()?;
        grid.screen_rows_text(held - self.offset, usize::from(rows))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A grid whose history is `line-0` through `line-{n-1}`, each printed on
    /// its own row, with the viewport holding the tail of the run.
    fn numbered(cols: u16, rows: u16, scrollback: usize, lines: usize) -> Grid {
        let mut grid = Grid::with_scrollback(cols, rows, scrollback).expect("a valid grid");
        for i in 0..lines {
            grid.write(format!("line-{i}\r\n").as_bytes());
        }
        grid
    }

    #[test]
    fn scroll_at_the_live_edge_the_view_reads_the_live_viewport() {
        let grid = numbered(20, 5, 100, 12);
        let mut view = HistoryView::new();

        // The window is the VIEWPORT, not `Grid::text` — that renders every
        // retained row, which is exactly what a scroll view exists to avoid.
        assert!(view.is_live());
        let text = view.text(&grid).expect("a live read");
        let rows: Vec<&str> = text.lines().collect();
        assert_eq!(rows.first().copied(), Some("line-8"), "the top live row");
        assert_eq!(rows.last().copied(), Some("line-11"), "the live tail");
    }

    #[test]
    fn scroll_up_reads_older_rows_and_stops_at_the_oldest() {
        let grid = numbered(20, 5, 100, 12);
        let mut view = HistoryView::new();

        // 12 lines into a 5-row viewport: the shell-style trailing blank line
        // sits on the last row, so 8 rows of history hold line-0..line-7.
        view.scroll_up(3, &grid);
        assert!(!view.is_live());
        let text = view.text(&grid).expect("a scrolled read");
        assert!(
            text.starts_with("line-5"),
            "3 rows up from line-8: {text:?}"
        );

        // Overshooting clamps to the top rather than erroring.
        view.scroll_up(usize::MAX, &grid);
        assert_eq!(view.offset(), grid.scrollback_rows().expect("history"));
        let top = view.text(&grid).expect("the top read");
        assert!(
            top.starts_with("line-0"),
            "the oldest retained row: {top:?}"
        );

        let mut jumped = HistoryView::new();
        jumped.to_top(&grid);
        assert_eq!(jumped, view, "to_top is the overshoot's destination");
    }

    #[test]
    fn scroll_down_returns_to_live_and_saturates() {
        let grid = numbered(20, 5, 100, 12);
        let mut view = HistoryView::new();

        view.to_top(&grid);
        view.scroll_down(usize::MAX);
        assert!(view.is_live());
        view.scroll_down(1);
        assert!(view.is_live(), "below live does not exist");

        view.to_top(&grid);
        view.to_live();
        assert!(view.is_live());
    }

    #[test]
    fn scroll_a_held_view_survives_eviction_and_reflow_by_clamping() {
        let mut grid = numbered(20, 5, 8, 12);
        let mut view = HistoryView::new();
        view.to_top(&grid);

        // Eviction: enough new lines that every row the view was over is
        // evicted out of the 8-row history budget. The offset still names a
        // depth that exists, so the read succeeds over the surviving rows.
        for i in 12..40 {
            grid.write(format!("line-{i}\r\n").as_bytes());
        }
        let text = view.text(&grid).expect("a read, not a refusal");
        assert_eq!(text.lines().count(), 5, "a viewport-sized window");
        assert!(view.offset() <= grid.scrollback_rows().expect("history"));

        // Reflow: growing the viewport pulls rows out of history, and THAT
        // can leave the held offset deeper than what remains — the case the
        // clamp exists for.
        view.to_top(&grid);
        grid.resize(20, 10).expect("a valid resize");
        let text = view.text(&grid).expect("a clamped read, not a refusal");
        assert_eq!(text.lines().count(), 10, "the new viewport size");
        assert!(view.offset() <= grid.scrollback_rows().expect("history"));
    }

    #[test]
    fn scroll_reading_history_disturbs_nothing_live() {
        let grid = numbered(20, 5, 100, 12);
        let before_text = grid.text().expect("the live screen");
        let before_cursor = grid.cursor().expect("the live cursor");

        let mut view = HistoryView::new();
        view.to_top(&grid);
        view.text(&grid).expect("a deep read");

        assert_eq!(grid.text().expect("after the read"), before_text);
        assert_eq!(grid.cursor().expect("after the read"), before_cursor);
    }
}
