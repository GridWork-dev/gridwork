//! Engine output to wire data: the conversion `gwk-domain`'s frame module
//! deliberately does not do itself (its module doc: "the engine-side
//! conversion from a real terminal grid into this shape is a later task").
//! This is that task's home.
//!
//! [`WireScreen`] is one session's wire-shaped screen state — the mirror the
//! host keeps so a [`PtySnapshot`](gwk_domain::protocol::KernelRequest::PtySnapshot)
//! can be answered at any moment. The engine's [`Renderer`] reports only the
//! rows that changed and consumes the grid's dirty state as it does, so
//! there is no second chance to ask the engine for a full styled screen;
//! applying every frame into this mirror is what keeps one.
//!
//! Derivation: none — converts the engine crate's own typed output into the
//! wire contract's types field by field; no terminal-protocol byte is parsed
//! and no process is supervised here. The one non-obvious mapping (a palette
//! index never becomes `Ansi16`) is a recorded design ruling, not a spec
//! fact — see [`color`].

use gwk_domain::frame::{
    CellColor, CellStyle as WireStyle, CellUnderline, PtyCellUpdate, PtyCursor, PtyDelta, PtyFrame,
    StyleInterner, StyledCell,
};
use gwk_pty::render::{Frame, Row};
use gwk_pty::style::{CellStyle as EngineStyle, Color, Underline};

/// One engine color, in the wire's vocabulary.
///
/// `Palette` maps to `Xterm256` UNIFORMLY, indices 0-15 included — never to
/// `Ansi16`. The engine holds "the raw index, never the decoded meaning"
/// (`gwk_pty::style::Color`'s own doc), and the SGR form a child used to
/// select an index below 16 (`31` vs `38;5;1`) is exactly what the engine
/// discarded — mapping those indices to `Ansi16` would claim a distinction
/// the data does not carry. This is the design answer task 15's PLAN note
/// recorded, and its cost is stated there too: `CellColor::Ansi16` is
/// unreachable from the engine path until the engine preserves the SGR form.
pub fn color(color: Color) -> CellColor {
    match color {
        Color::Palette(index) => CellColor::Xterm256 { index },
        Color::Rgb(r, g, b) => CellColor::Truecolor { r, g, b },
    }
}

/// One engine underline shape, in the wire's vocabulary. Total in both
/// directions — the wire enum's doc says the variants "match what
/// `gwk_pty::Underline` carries" and this match is where that claim is code.
pub fn underline(underline: Underline) -> CellUnderline {
    match underline {
        Underline::Single => CellUnderline::Single,
        Underline::Double => CellUnderline::Double,
        Underline::Curly => CellUnderline::Curly,
        Underline::Dotted => CellUnderline::Dotted,
        Underline::Dashed => CellUnderline::Dashed,
    }
}

/// One engine cell style, in the wire's vocabulary. Field-for-field: both
/// sides carry the same twelve facts by construction (the wire struct's doc
/// binds it to `gwk_pty::CellStyle`), so this is a rename, not a projection.
pub fn style(style: &EngineStyle) -> WireStyle {
    WireStyle {
        bold: style.bold,
        dim: style.dim,
        italic: style.italic,
        blink: style.blink,
        inverse: style.inverse,
        invisible: style.invisible,
        strikethrough: style.strikethrough,
        overline: style.overline,
        underline: style.underline.map(underline),
        fg: style.fg.map(color),
        bg: style.bg.map(color),
        underline_color: style.underline_color.map(color),
    }
}

/// A blank cell: what a fresh screen is full of. The glyph is a space, not
/// an empty string, matching what the engine's own row text reports for a
/// never-written cell — the empty string is reserved for a wide cluster's
/// trailing half, per `StyledCell`'s doc.
fn blank() -> StyledCell {
    StyledCell {
        glyph: " ".to_owned(),
        style: WireStyle {
            bold: false,
            dim: false,
            italic: false,
            blink: false,
            inverse: false,
            invisible: false,
            strikethrough: false,
            overline: false,
            underline: None,
            fg: None,
            bg: None,
            underline_color: None,
        },
    }
}

/// One session's screen as the wire sees it, plus the deltas that keep a
/// consumer's copy current.
pub struct WireScreen {
    cells: Vec<Vec<StyledCell>>,
    cursor: Option<PtyCursor>,
}

impl WireScreen {
    /// A blank screen of `cols` × `rows`.
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            cells: vec![vec![blank(); usize::from(cols)]; usize::from(rows)],
            cursor: None,
        }
    }

    /// `(cols, rows)`.
    pub fn size(&self) -> (u16, u16) {
        let rows = self.cells.len() as u16;
        let cols = self.cells.first().map_or(0, |row| row.len() as u16);
        (cols, rows)
    }

    /// The full screen, for a `PtySnapshot` answer — interned into the wire
    /// form: a first-appearance style table, maximal same-style runs per row
    /// (the `from_cells` canonical form), which is what turns a blank screen
    /// from megabytes of repeated style objects into kilobytes of fills.
    pub fn snapshot(&self) -> PtyFrame {
        PtyFrame::from_cells(&self.cells, self.cursor)
    }

    /// Apply one engine frame; the returned deltas bring a consumer holding
    /// the previous revision to this one.
    ///
    /// A frame whose dimensions differ from the mirror's is a resize: the
    /// mirror is rebuilt blank at the new size and the deltas lead with
    /// [`PtyDelta::Resized`], matching the wire contract's rule that a resize
    /// invalidates every coordinate a client holds. The engine always follows
    /// a real resize with a full repaint (its dirty flag goes `Full`), so the
    /// blank rebuild is repainted in the same batch.
    pub fn apply(&mut self, frame: &Frame) -> Vec<PtyDelta> {
        let mut deltas = Vec::new();
        if (frame.cols, frame.rows) != self.size() {
            *self = Self::new(frame.cols, frame.rows);
            deltas.push(PtyDelta::Resized {
                rows: frame.rows,
                cols: frame.cols,
            });
        }
        self.cursor = frame.cursor.map(|(col, row)| PtyCursor { row, col });
        // The batch's own style table, built as the updates are: every
        // message self-contained, no index space outliving it.
        let mut styles = StyleInterner::default();
        let mut updates = Vec::new();
        for row in &frame.changed {
            self.apply_row(row, &mut styles, &mut updates);
        }
        if !updates.is_empty() || deltas.is_empty() {
            deltas.push(PtyDelta::CellsChanged {
                styles: styles.into_styles(),
                updates,
            });
        }
        deltas
    }

    /// Expand one changed row into per-column cell updates and mirror them.
    ///
    /// The engine reports one cluster per non-spacer cell with its starting
    /// column (`Row::cols`); the columns a cluster skips are its spacer
    /// cells — the trailing half of a wide glyph — which the wire carries as
    /// empty-glyph cells styled like their head, per `StyledCell`'s doc.
    fn apply_row(
        &mut self,
        row: &Row,
        styles: &mut StyleInterner,
        updates: &mut Vec<PtyCellUpdate>,
    ) {
        let Some(mirror_row) = self.cells.get_mut(usize::from(row.y)) else {
            // A row outside the mirror is a frame racing a resize; the full
            // repaint that follows the resize will carry it. Dropping it is
            // safe BECAUSE the mirror and the deltas move together — the
            // consumer's copy is exactly this mirror, so a cell the mirror
            // never took is a cell no consumer was told about either.
            return;
        };
        let width = mirror_row.len();
        let mut text_clusters = split_clusters(&row.text, &row.cols);

        for i in 0..row.cols.len() {
            let (Some(&col), Some(style_entry)) = (row.cols.get(i), row.styles.get(i)) else {
                break;
            };
            let glyph = text_clusters.next().unwrap_or_default();
            let wire_style = style(style_entry);
            let end = row
                .cols
                .get(i + 1)
                .map_or(width, |&next| usize::from(next).min(width));
            let start = usize::from(col);
            if start >= width {
                break;
            }
            set_cell(
                mirror_row,
                row.y,
                start,
                StyledCell {
                    glyph,
                    style: wire_style.clone(),
                },
                styles,
                updates,
            );
            // The spacer columns this cluster covers: empty glyph, same
            // style, exactly the shape `StyledCell` documents for the
            // trailing half of a double-width character.
            for spacer in (start + 1)..end {
                set_cell(
                    mirror_row,
                    row.y,
                    spacer,
                    StyledCell {
                        glyph: String::new(),
                        style: wire_style.clone(),
                    },
                    styles,
                    updates,
                );
            }
        }
    }
}

/// Write one cell into the mirror and record the update — but only when it
/// changed. The engine's row granularity means an edited row re-reports every
/// cell in it; forwarding the unchanged ones would make every keystroke cost
/// a row's worth of wire traffic. Only a CHANGED cell interns its style, so
/// the batch table carries exactly the styles its updates reference.
fn set_cell(
    mirror_row: &mut [StyledCell],
    y: u16,
    col: usize,
    cell: StyledCell,
    styles: &mut StyleInterner,
    updates: &mut Vec<PtyCellUpdate>,
) {
    if mirror_row[col] == cell {
        return;
    }
    let style = styles.intern(&cell.style);
    updates.push(PtyCellUpdate {
        row: y,
        col: col as u16,
        glyph: cell.glyph.clone(),
        style,
    });
    mirror_row[col] = cell;
}

/// Split a row's text back into the clusters that were pushed into it.
///
/// The renderer pushed one cluster per `cols` entry, in order, and clusters
/// are whole `char` runs — so the split walks `text` taking one `char` per
/// entry EXCEPT where the engine put several codepoints in one cell, which
/// only happens when the next cluster's column says this one is still open.
/// That case cannot be recovered from column arithmetic alone (a wide
/// single-char cluster also spans two columns), so the split leans on the
/// one structural fact that IS reliable: there are exactly as many clusters
/// as `cols` entries, and the LAST cluster takes whatever text remains.
/// Interior multi-codepoint clusters are therefore split conservatively —
/// one `char` each — and the remainder folds into the final cluster.
///
/// ponytail: exact interior multi-codepoint cluster recovery would need the
/// engine to hand out per-cluster byte ranges. Add that field to `Row` the
/// day a consumer renders ZWJ sequences mid-row through this path; today the
/// mirror's glyphs still concatenate to the row's text, and every
/// single-codepoint case — which is all of ASCII and CJK — is exact.
fn split_clusters<'a>(text: &'a str, cols: &[u16]) -> impl Iterator<Item = String> + 'a {
    let clusters = cols.len();
    let mut chars = text.chars();
    let mut emitted = 0usize;
    std::iter::from_fn(move || {
        if emitted + 1 == clusters {
            emitted += 1;
            let rest: String = chars.by_ref().collect();
            return Some(rest);
        }
        if emitted >= clusters {
            return None;
        }
        emitted += 1;
        chars.next().map(String::from)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gwk_pty::{Grid, Renderer};

    /// Feed `input` to a fresh grid and return the deltas plus the screen.
    fn apply_once(cols: u16, rows: u16, input: &[u8]) -> (Vec<PtyDelta>, WireScreen) {
        let mut grid = Grid::new(cols, rows).expect("grid");
        let mut renderer = Renderer::new().expect("renderer");
        let mut screen = WireScreen::new(cols, rows);
        grid.write(input);
        let frame = renderer.frame(&grid, 0).expect("frame");
        let deltas = screen.apply(&frame);
        (deltas, screen)
    }

    fn glyph_at(screen: &WireScreen, row: usize, col: usize) -> &str {
        &screen.cells[row][col].glyph
    }

    #[test]
    fn palette_maps_to_xterm256_uniformly_including_the_low_sixteen() {
        // The recorded ruling: index 1 (a `31` on the child's wire) must NOT
        // become Ansi16, because the engine no longer knows whether the child
        // said `31` or `38;5;1`.
        assert_eq!(color(Color::Palette(1)), CellColor::Xterm256 { index: 1 });
        assert_eq!(
            color(Color::Palette(208)),
            CellColor::Xterm256 { index: 208 }
        );
        assert_eq!(
            color(Color::Rgb(10, 20, 30)),
            CellColor::Truecolor {
                r: 10,
                g: 20,
                b: 30
            }
        );
    }

    #[test]
    fn every_underline_shape_survives_the_conversion() {
        // Five in, five distinct out — a dropped arm would not compile (the
        // match is exhaustive), so what this pins is the PAIRING.
        for (engine, wire) in [
            (Underline::Single, CellUnderline::Single),
            (Underline::Double, CellUnderline::Double),
            (Underline::Curly, CellUnderline::Curly),
            (Underline::Dotted, CellUnderline::Dotted),
            (Underline::Dashed, CellUnderline::Dashed),
        ] {
            assert_eq!(underline(engine), wire);
        }
    }

    #[test]
    fn a_written_row_lands_in_the_mirror_and_the_updates_agree() {
        let (deltas, screen) = apply_once(10, 2, b"hi");
        assert_eq!(glyph_at(&screen, 0, 0), "h");
        assert_eq!(glyph_at(&screen, 0, 1), "i");
        assert_eq!(glyph_at(&screen, 0, 2), " ");

        // The updates describe exactly the mirror's non-blank state: a
        // consumer starting blank, resolving each update through the batch's
        // own style table, ends up at the mirror.
        let PtyDelta::CellsChanged { styles, updates } = &deltas[0] else {
            panic!("expected cell updates, got {deltas:?}");
        };
        let mut replayed = WireScreen::new(10, 2);
        for update in updates {
            replayed.cells[usize::from(update.row)][usize::from(update.col)] = StyledCell {
                glyph: update.glyph.clone(),
                style: styles[update.style as usize].clone(),
            };
        }
        assert_eq!(replayed.cells, screen.cells);
    }

    #[test]
    fn a_wide_glyph_occupies_its_head_column_and_an_empty_tail() {
        let (_, screen) = apply_once(6, 1, "a\u{5BEC}b".as_bytes());
        assert_eq!(glyph_at(&screen, 0, 0), "a");
        assert_eq!(glyph_at(&screen, 0, 1), "\u{5BEC}");
        assert_eq!(
            glyph_at(&screen, 0, 2),
            "",
            "the trailing half of a wide glyph is the empty string"
        );
        assert_eq!(glyph_at(&screen, 0, 3), "b");
    }

    #[test]
    fn styles_ride_the_conversion() {
        // The escape bytes are a fixture, not a claim: that SGR 1 means bold
        // and SGR 31 selects palette index 1 is pinned, with its citations,
        // in `crates/gwk-pty/tests/style.rs` (`bold_sets_the_bold_flag`,
        // `foreground_16_color_is_a_palette_index`). What this asserts is
        // only that the conversion preserves what the engine reported.
        let (_, screen) = apply_once(4, 1, b"\x1b[1;31mX");
        let cell = &screen.cells[0][0];
        assert!(cell.style.bold);
        assert_eq!(cell.style.fg, Some(CellColor::Xterm256 { index: 1 }));
    }

    #[test]
    fn a_resize_leads_the_deltas_and_rebuilds_the_mirror() {
        let mut grid = Grid::new(10, 2).expect("grid");
        let mut renderer = Renderer::new().expect("renderer");
        let mut screen = WireScreen::new(10, 2);
        grid.write(b"hello");
        screen.apply(&renderer.frame(&grid, 0).expect("frame"));

        grid.resize(20, 4).expect("resize");
        let deltas = screen.apply(&renderer.frame(&grid, 1).expect("frame"));

        assert!(
            matches!(deltas[0], PtyDelta::Resized { rows: 4, cols: 20 }),
            "a resize must lead the batch: {deltas:?}"
        );
        assert_eq!(screen.size(), (20, 4));
        assert_eq!(
            glyph_at(&screen, 0, 0),
            "h",
            "the engine's full repaint should have refilled the rebuilt mirror"
        );
    }

    #[test]
    fn an_unchanged_screen_produces_no_updates() {
        let mut grid = Grid::new(10, 1).expect("grid");
        let mut renderer = Renderer::new().expect("renderer");
        let mut screen = WireScreen::new(10, 1);
        grid.write(b"x");
        screen.apply(&renderer.frame(&grid, 0).expect("frame"));

        // Nothing written since: the frame is clean and the delta batch,
        // when one is produced at all, carries nothing.
        let deltas = screen.apply(&renderer.frame(&grid, 1).expect("frame"));
        for delta in &deltas {
            if let PtyDelta::CellsChanged { styles, updates } = delta {
                assert!(updates.is_empty(), "unchanged cells re-sent: {updates:?}");
                assert!(styles.is_empty(), "an empty batch interned styles anyway");
            }
        }
    }

    #[test]
    fn snapshot_is_rectangular_and_matches_the_mirror() {
        let (_, screen) = apply_once(8, 3, b"one\r\ntwo");
        let frame = screen.snapshot();
        let cells = frame.cells().expect("a snapshot expands");
        assert_eq!(cells.len(), 3);
        assert!(cells.iter().all(|row| row.len() == 8));
        assert_eq!(cells, screen.cells);
    }

    #[test]
    fn snapshot_converts_visible_cursor_coordinates_and_omits_hidden_cursor() {
        let mut screen = WireScreen::new(8, 3);
        screen.apply(&Frame {
            seq: 0,
            cols: 8,
            rows: 3,
            cursor: Some((4, 2)),
            full: false,
            changed: Vec::new(),
        });
        assert_eq!(screen.snapshot().cursor, Some(PtyCursor { row: 2, col: 4 }));

        screen.apply(&Frame {
            seq: 1,
            cols: 8,
            rows: 3,
            cursor: None,
            full: false,
            changed: Vec::new(),
        });
        assert_eq!(screen.snapshot().cursor, None);
    }

    // ---- The size floor the interned shape exists for (P16) ----
    //
    // The spike measured the per-cell shape at 151.009-151.031 bytes/cell:
    // blank 240x70 = 2,536,951 bytes, busting the 2,097,152-byte publish
    // budget alone; attr-heavy 200x60 = 4,032,131. These tests pin the
    // interned shape against those geometries with real serialization.

    /// Half of `FRAME_BODY_MAX_BYTES`, as the kernel and `publish` both
    /// derive it; recomputed here so a size test compares against the same
    /// number the wire enforces.
    fn publish_byte_budget() -> usize {
        usize::try_from(gwk_domain::protocol::FRAME_BODY_MAX_BYTES).expect("fits usize") / 2
    }

    fn measured(screen: &WireScreen) -> usize {
        serde_json::to_vec(&screen.snapshot())
            .expect("serialize")
            .len()
    }

    #[test]
    fn a_blank_240x70_frame_serializes_in_kilobytes_not_megabytes() {
        // Was 2,536,951 bytes — 439,799 past the budget before any envelope
        // existed. One blank style plus 70 fill runs now. The bound leaves
        // headroom over the expected ~3.5 KB so a serde or style-field
        // change cannot flap it, while any per-cell regression (which lands
        // back at megabytes) still fails loudly.
        let bytes = measured(&WireScreen::new(240, 70));
        assert!(
            bytes < 16 * 1024,
            "{bytes} bytes: the interned blank frame regressed toward per-cell size"
        );
    }

    #[test]
    fn eight_blank_80x24_screens_fit_one_publish_budget_together() {
        // The B4-floor arrangement the spike measured OVER at 8 sessions
        // (2,319,832 bytes): eight blank 80x24 screens together now sit far
        // inside one publish budget.
        let bytes = measured(&WireScreen::new(80, 24));
        assert!(
            bytes * 8 <= publish_byte_budget(),
            "8 x {bytes} bytes busts the {}-byte budget",
            publish_byte_budget()
        );
    }

    #[test]
    fn the_attr_heavy_fixture_re_measured_under_the_interned_shape() {
        // The spike's synthetic attr-heavy fixture, driven through the same
        // shipped path (Grid -> Renderer -> WireScreen): every cell an `X`
        // written through the real VT parser with all eight boolean
        // attributes, a single underline, and independent three-digit
        // truecolor fg/bg/underline colors. 200x60 measured 4,032,131 bytes
        // under the per-cell shape; interned it is one style table entry
        // plus 60 uniform fill rows.
        use std::fmt::Write as _;
        let mut input = String::new();
        input.push_str(
            "\x1b[1;2;3;4;5;7;8;9;53m\x1b[38;2;210;220;230m\x1b[48;2;110;120;130m\x1b[58:2::140:150:160m",
        );
        for row in 1..=60 {
            write!(&mut input, "\x1b[{row};1H").expect("write a row address");
            input.push_str(&"X".repeat(200));
        }
        let (_, screen) = apply_once(200, 60, input.as_bytes());
        let bytes = measured(&screen);
        // Re-measured 2026-08-11 after the visible cursor joined the frame:
        // 3,485 bytes (was 3,455 without the cursor, 4,032,131 per-cell).
        // Pinned exactly so every wire-shape move is reviewed, not absorbed.
        assert_eq!(bytes, 3_485, "the attr-heavy fixture moved: {bytes} bytes");
        assert!(bytes <= publish_byte_budget());
    }

    #[test]
    fn the_worst_legal_case_still_serializes_and_the_budget_judges_it_by_bytes() {
        // Every cell a DISTINCT style — the one screen interning cannot
        // compress. 200x60 = 12,000 table entries plus 12,000 width-1 runs,
        // roughly today's per-cell size plus table overhead. It must still
        // serialize, and whether the budget admits it is exactly what the
        // actual-bytes rule says — no arithmetic shortcut on either side.
        let mut screen = WireScreen::new(200, 60);
        for (y, row) in screen.cells.iter_mut().enumerate() {
            for (x, cell) in row.iter_mut().enumerate() {
                cell.glyph = "X".to_owned();
                cell.style.fg = Some(CellColor::Truecolor {
                    r: y as u8,
                    g: (x % 200) as u8,
                    b: (x / 200) as u8,
                });
            }
        }
        let frame = screen.snapshot();
        assert_eq!(frame.styles.len(), 200 * 60, "every style distinct");
        let bytes = serde_json::to_vec(&frame).expect("still serializes").len();
        // Re-measured 2026-08-09: 2,716,431 bytes (was 4,032,131 with the
        // same per-cell payload) — over the 2,097,152-byte budget, so the
        // actual-bytes rule refuses this screen exactly as it did before
        // the interning landed.
        assert_eq!(
            bytes, 2_716_431,
            "the worst legal case moved: {bytes} bytes"
        );
        assert!(bytes > publish_byte_budget());
    }
}
