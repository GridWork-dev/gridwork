//! The styled-cell wire shape for a hosted PTY session's rendered grid.
//!
//! Self-contained on purpose: this crate does not depend on `crates/gwk-pty`
//! (the VT engine that will eventually PRODUCE these values) or on
//! `gwk-theme` (the TUI's own design tokens, a different concern — those
//! paint the shell around a session, these carry exactly what the child
//! process's escape sequences said). The engine-side conversion from a real
//! terminal grid into this shape is a later task; this module only fixes the
//! wire contract it converts INTO.
//!
//! # The interned form
//!
//! A frame does not carry a style object per cell. It carries every distinct
//! [`CellStyle`] exactly once in a frame-scoped table, and each row as a run
//! list referencing that table by index — because the style object measured
//! as 85–93% of every cell's serialized bytes, and a screen repeats a
//! handful of styles across thousands of cells. Deltas intern the same way,
//! with a batch-scoped table: a full-screen repaint arrives as one
//! `cells_changed` batch, and without its own table it would re-pay the
//! per-cell style cost the frame shape just removed. No index space ever
//! crosses a message boundary — every frame and every batch is
//! self-contained, so a late joiner is correct by construction and no
//! consumer holds a persistent table.
//!
//! [`PtyAnsiSlot`] duplicates `gwk_theme::tier::AnsiSlot`'s sixteen names
//! deliberately rather than sharing it: that type is a THEME token's
//! rendering disposition, this one is the raw ANSI slot a wire cell reports.
//! The two will keep agreeing on names because both describe the same
//! sixteen terminal slots, but they answer different questions and a shared
//! dependency would couple them for no reason — and specta's TypeScript
//! export refuses two registered types that share a bare name, so a shared
//! name here would not even compile once both crates reach `bindings.ts`.
//!
//! Derivation: none — original wire encoding over this repository's own
//! existing style type; no terminal-protocol byte is parsed and no new
//! colour, SGR, or keyboard fact is asserted anywhere in this module.

use std::collections::HashMap;

/// One of the sixteen slots a terminal's own theme owns. Declaration order IS
/// slot order — pinned by `ansi_slot_declaration_order_is_slot_index`.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    specta::Type,
)]
#[serde(rename_all = "snake_case")]
pub enum PtyAnsiSlot {
    Black,
    Red,
    Green,
    Yellow,
    Blue,
    Magenta,
    Cyan,
    White,
    BrightBlack,
    BrightRed,
    BrightGreen,
    BrightYellow,
    BrightBlue,
    BrightMagenta,
    BrightCyan,
    BrightWhite,
}

impl PtyAnsiSlot {
    /// Every slot, in wire/index order.
    #[rustfmt::skip]
    pub const ALL: &'static [PtyAnsiSlot] = &[
        PtyAnsiSlot::Black, PtyAnsiSlot::Red, PtyAnsiSlot::Green, PtyAnsiSlot::Yellow,
        PtyAnsiSlot::Blue, PtyAnsiSlot::Magenta, PtyAnsiSlot::Cyan, PtyAnsiSlot::White,
        PtyAnsiSlot::BrightBlack, PtyAnsiSlot::BrightRed, PtyAnsiSlot::BrightGreen, PtyAnsiSlot::BrightYellow,
        PtyAnsiSlot::BrightBlue, PtyAnsiSlot::BrightMagenta, PtyAnsiSlot::BrightCyan, PtyAnsiSlot::BrightWhite,
    ];

    /// 0-15.
    pub const fn index(self) -> u8 {
        self as u8
    }

    /// The wire value — identical to what serde emits.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Black => "black",
            Self::Red => "red",
            Self::Green => "green",
            Self::Yellow => "yellow",
            Self::Blue => "blue",
            Self::Magenta => "magenta",
            Self::Cyan => "cyan",
            Self::White => "white",
            Self::BrightBlack => "bright_black",
            Self::BrightRed => "bright_red",
            Self::BrightGreen => "bright_green",
            Self::BrightYellow => "bright_yellow",
            Self::BrightBlue => "bright_blue",
            Self::BrightMagenta => "bright_magenta",
            Self::BrightCyan => "bright_cyan",
            Self::BrightWhite => "bright_white",
        }
    }
}

/// One color a cell's foreground or background carries, in the tier the
/// terminal engine reported it at — exactly what the engine parsed off the
/// child process's escape sequences, never re-interpreted against a theme.
/// The three tiers name themselves after `gwk_theme::tier::ColorTier`'s
/// non-mono tiers by deliberate analogy (see the module doc); there is no
/// mono variant because mono is a RENDERING choice a client makes, not a fact
/// the wire carries.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, specta::Type,
)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CellColor {
    /// One of the terminal's sixteen user-themed slots.
    Ansi16 { slot: PtyAnsiSlot },
    /// The 256-color palette index: 0-15 alias the ANSI slots, 16-231 are the
    /// 6x6x6 cube, 232-255 are the grayscale ramp. The wire carries the raw
    /// index, never the decoded meaning.
    Xterm256 { index: u8 },
    /// 24-bit truecolor.
    Truecolor { r: u8, g: u8, b: u8 },
}

/// How a cell's text is underlined, when it is.
///
/// A separate enum rather than a `bool` because the shape is a real attribute
/// the engine reports, not a rendering flourish: a terminal that draws a curly
/// underline where the child asked for a dotted one is displaying something
/// the child did not send. The variants match what `gwk_pty::Underline`
/// carries, so the engine-to-wire conversion is total in both directions.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, specta::Type,
)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum CellUnderline {
    Single,
    Double,
    Curly,
    Dotted,
    Dashed,
}

/// The attribute bits and colors a cell carries, independent of its glyph.
///
/// This is the object the wire interns: a frame or a delta batch carries each
/// distinct value of this struct exactly once in its `styles` table, and
/// every run or update references it by index. The struct itself is unchanged
/// by that move — the table stores this existing type verbatim.
///
/// `Hash` exists for exactly that interning (a first-appearance
/// `HashMap<CellStyle, u32>` in [`StyleInterner`]), not for any wire fact.
///
/// The eight attributes below are plain, always-present booleans — not
/// `Option<bool>` — matching how this contract already carries `dirty` and
/// `unpushed` elsewhere (`docs/contract/NAMING.md`): a boolean fact has no
/// third "absent" state to distinguish from `false`.
///
/// `underline` is the exception, and deliberately so: it is not a boolean
/// fact. A cell is un-underlined or carries one of five shapes, which is six
/// states, so it is an optional [`CellUnderline`] and absent means "not
/// underlined" the same way an absent `fg` means the terminal's own default.
/// An earlier version of this struct flattened it to a `bool`, which reported
/// a `4:3` curly underline as indistinguishable from a plain `4` — a fidelity
/// loss at the wire, in a contract whose whole job is to carry what the engine
/// parsed rather than what a client would guess.
///
/// The set here tracks `gwk_pty::CellStyle`. When the engine gains an
/// attribute this struct does not carry, a consumer attaching to a session
/// silently loses it — so the two move together, and a widening of one without
/// the other is the defect, not the fix.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct CellStyle {
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub blink: bool,
    pub inverse: bool,
    pub invisible: bool,
    pub strikethrough: bool,
    pub overline: bool,
    /// Absent means the cell is not underlined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub underline: Option<CellUnderline>,
    /// Absent means the terminal's own default foreground.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub fg: Option<CellColor>,
    /// Absent means the terminal's own default background.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub bg: Option<CellColor>,
    /// The underline's own color, set independently of the text's foreground.
    /// Absent means it follows the foreground, which is also what an
    /// un-underlined cell carries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub underline_color: Option<CellColor>,
}

/// One terminal cell: a displayed glyph plus the style it carries.
///
/// NOT a wire type since the interned form landed: the wire carries glyphs
/// inside [`PtyRun`]s and styles in a table, and this struct is what a
/// decoded frame EXPANDS to — the per-cell model every mirror and render
/// path keeps ([`PtyFrame::cells`] produces it, [`PtyFrame::from_cells`]
/// consumes it). The wire shape stops at the decode boundary; this is what
/// lives on the other side.
///
/// `glyph` is a `String`, not a `char` — a cell can hold a full grapheme
/// cluster (a combining mark, a ZWJ emoji sequence), which a single Rust
/// `char` cannot represent. An empty string is a legal glyph: a blank cell,
/// or the trailing half of a double-width character. Column width — a wide
/// glyph occupies two grid cells — is deliberately NOT modeled here: deciding
/// it is part of the engine-side conversion this crate defers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StyledCell {
    pub glyph: String,
    pub style: CellStyle,
}

/// One run of consecutive same-style cells inside a row.
///
/// Two carriers, one meaning: `width()` cells sharing one interned style.
/// `Cells` holds per-cell glyphs, NOT a joined string — the wire deliberately
/// does not model column width, so a joined string could never be re-split
/// into cells (an empty glyph is a wide cluster's trailing half, and nothing
/// in the text itself says where it sits). `Fill` is the blank-screen
/// carrier: one glyph repeated `count` cells, which is what makes an
/// all-blank row O(1) bytes instead of O(cols).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PtyRun {
    /// Consecutive same-style cells, one glyph per cell.
    Cells { style: u32, glyphs: Vec<String> },
    /// One glyph repeated `count` cells.
    Fill {
        style: u32,
        glyph: String,
        count: u32,
    },
}

impl PtyRun {
    /// The number of grid cells this run covers.
    pub fn width(&self) -> usize {
        match self {
            Self::Cells { glyphs, .. } => glyphs.len(),
            Self::Fill { count, .. } => *count as usize,
        }
    }

    /// The run's index into its frame's `styles` table.
    pub const fn style_index(&self) -> u32 {
        match self {
            Self::Cells { style, .. } | Self::Fill { style, .. } => *style,
        }
    }
}

/// A full styled frame: every cell of a hosted PTY session's grid at one
/// [`crate::ids::PtyFrameSeq`], as a frame-scoped style table plus row-major
/// runs (`rows[row]` = that row's runs, left to right).
///
/// `rows`/`cols` are not stored as separate fields — they are exactly
/// `rows.len()` and each row's summed run width, every row the same width,
/// and this type has no constructor that could let a stored pair disagree
/// with the runs it describes. That shape rule — and every other invariant
/// the type admits but the contract refuses (style indices in range, no
/// duplicate table entry, no zero-width run) — is the kernel wire layer's to
/// enforce (see `crate::protocol`'s module doc on the split between what a
/// type admits and what the strict decoder refuses), not this type's to
/// self-validate: a wire type validates itself only when its value set is
/// CLOSED, and a rectangular grid of arbitrary size is not.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct PtyFrame {
    /// Every distinct style in the frame, exactly once, first-appearance
    /// order.
    pub styles: Vec<CellStyle>,
    /// `rows[row]` = the row's runs, left to right.
    pub rows: Vec<Vec<PtyRun>>,
}

impl PtyFrame {
    /// Intern an expanded grid into the wire form: a first-appearance style
    /// table and maximal same-style runs per row, with a run that repeats one
    /// glyph carried as [`PtyRun::Fill`] and every other run as
    /// [`PtyRun::Cells`].
    ///
    /// This is the canonical producer form: adjacent runs never share a
    /// style, so a decoder holding the tolerate-non-maximal ruling still
    /// never sees a non-maximal run from this constructor.
    pub fn from_cells(cells: &[Vec<StyledCell>]) -> Self {
        let mut interner = StyleInterner::default();
        let mut rows = Vec::with_capacity(cells.len());
        for row in cells {
            let mut runs: Vec<PtyRun> = Vec::new();
            let mut start = 0usize;
            while start < row.len() {
                let style = &row[start].style;
                let mut end = start + 1;
                while end < row.len() && &row[end].style == style {
                    end += 1;
                }
                let index = interner.intern(style);
                let run = &row[start..end];
                let uniform = run.iter().all(|cell| cell.glyph == run[0].glyph);
                runs.push(if uniform {
                    PtyRun::Fill {
                        style: index,
                        glyph: run[0].glyph.clone(),
                        // A row is far below u32::MAX cells; the expansion
                        // bound in `cells()` is what holds the other side.
                        count: run.len() as u32,
                    }
                } else {
                    PtyRun::Cells {
                        style: index,
                        glyphs: run.iter().map(|cell| cell.glyph.clone()).collect(),
                    }
                });
                start = end;
            }
            rows.push(runs);
        }
        Self {
            styles: interner.into_styles(),
            rows,
        }
    }

    /// Expand back to the per-cell grid the runs describe, or `None` for a
    /// frame no rectangular grid matches: a style index outside the table,
    /// rows of unequal width, or a dimension past `u16` (the wire's grid
    /// universe — and the bound that stops a small `fill` run from demanding
    /// an enormous allocation).
    ///
    /// Deliberately more tolerant than the kernel's strict decoder: a
    /// duplicate table entry, a zero-width run, or two adjacent runs sharing
    /// a style all expand fine and are canonicality concerns, not expansion
    /// blockers — refusing them is the kernel wire layer's job, where the
    /// refusal has an error vocabulary to answer in.
    pub fn cells(&self) -> Option<Vec<Vec<StyledCell>>> {
        if self.rows.len() > usize::from(u16::MAX) {
            return None;
        }
        let mut cells = Vec::with_capacity(self.rows.len());
        let mut width: Option<usize> = None;
        for row in &self.rows {
            // Width and index checks before this row allocates anything: a
            // `fill` naming billions of cells must die as arithmetic.
            let mut total = 0u64;
            for run in row {
                if usize::try_from(run.style_index())
                    .ok()
                    .is_none_or(|index| index >= self.styles.len())
                {
                    return None;
                }
                total += run.width() as u64;
            }
            if total > u64::from(u16::MAX) {
                return None;
            }
            let total = total as usize;
            if *width.get_or_insert(total) != total {
                return None;
            }
            let mut expanded = Vec::with_capacity(total);
            for run in row {
                let style = &self.styles[run.style_index() as usize];
                match run {
                    PtyRun::Cells { glyphs, .. } => {
                        expanded.extend(glyphs.iter().map(|glyph| StyledCell {
                            glyph: glyph.clone(),
                            style: style.clone(),
                        }));
                    }
                    PtyRun::Fill { glyph, count, .. } => {
                        expanded.extend((0..*count).map(|_| StyledCell {
                            glyph: glyph.clone(),
                            style: style.clone(),
                        }));
                    }
                }
            }
            cells.push(expanded);
        }
        Some(cells)
    }
}

/// A first-appearance style table under construction — the
/// `HashMap<CellStyle, u32>` interning every producer of the wire shape
/// shares: [`PtyFrame::from_cells`] for frames, the host's delta batching
/// for updates.
#[derive(Debug, Default)]
pub struct StyleInterner {
    styles: Vec<CellStyle>,
    index: HashMap<CellStyle, u32>,
}

impl StyleInterner {
    /// The table index for `style`, minting the next first-appearance slot on
    /// first sight.
    pub fn intern(&mut self, style: &CellStyle) -> u32 {
        if let Some(&index) = self.index.get(style) {
            return index;
        }
        // A table cannot outgrow its frame's cell count, and a u16-bounded
        // grid holds fewer than 2^32 cells.
        let index = self.styles.len() as u32;
        self.styles.push(style.clone());
        self.index.insert(style.clone(), index);
        index
    }

    /// The finished table, first-appearance order.
    pub fn into_styles(self) -> Vec<CellStyle> {
        self.styles
    }
}

/// One cell's new content, addressed by zero-indexed grid position. `style`
/// indexes the CARRYING batch's `styles` table — never any other message's.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct PtyCellUpdate {
    pub row: u16,
    pub col: u16,
    pub glyph: String,
    pub style: u32,
}

/// One incremental change since the previous frame revision.
///
/// Two kinds, deliberately not one: a resize invalidates every coordinate a
/// client is already holding, so it cannot be folded into a cell update
/// without also telling the client the grid itself grew or shrank. Cursor
/// position and visibility are NOT modeled here — out of scope for this pass
/// (see the crate's kernel-protocol tests for what is pinned); a client
/// tracking those needs is a gap to close in the engine-side task, not a
/// silent omission this type hides.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum PtyDelta {
    /// Zero or more cells changed. Each update's `row`/`col` is valid against
    /// the grid's CURRENT size — the most recent snapshot or `resized` delta
    /// — and each update's `style` indexes this batch's own `styles` table.
    /// The table is batch-scoped so every message is self-contained: no index
    /// space ever crosses a message boundary, a late joiner via catch-up is
    /// correct by construction, and no mirror holds a persistent table.
    CellsChanged {
        styles: Vec<CellStyle>,
        updates: Vec<PtyCellUpdate>,
    },
    /// The session's grid was resized. Cell content outside the new bounds is
    /// gone; a client that needs the new content re-requests
    /// [`crate::protocol::KernelRequest::PtySnapshot`].
    Resized { rows: u16, cols: u16 },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style() -> CellStyle {
        CellStyle {
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
        }
    }

    fn cell(glyph: &str) -> StyledCell {
        StyledCell {
            glyph: glyph.to_owned(),
            style: style(),
        }
    }

    fn bold(glyph: &str) -> StyledCell {
        StyledCell {
            glyph: glyph.to_owned(),
            style: CellStyle {
                bold: true,
                ..style()
            },
        }
    }

    #[test]
    fn ansi_slot_declaration_order_is_slot_index() {
        for (expected, slot) in PtyAnsiSlot::ALL.iter().enumerate() {
            assert_eq!(slot.index() as usize, expected, "{slot:?}");
        }
        assert_eq!(PtyAnsiSlot::ALL.len(), 16);
    }

    #[test]
    fn ansi_slot_wire_values_are_snake_case_and_agree_with_serde() {
        for slot in PtyAnsiSlot::ALL {
            assert_eq!(
                serde_json::to_value(slot).expect("serialize"),
                serde_json::json!(slot.as_str()),
                "as_str() disagrees with serde for {slot:?}"
            );
            assert!(
                slot.as_str()
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{slot:?} is not snake_case"
            );
        }
        assert_eq!(PtyAnsiSlot::BrightRed.as_str(), "bright_red");
    }

    #[test]
    fn cell_color_tiers_round_trip_with_their_type_tags() {
        for color in [
            CellColor::Ansi16 {
                slot: PtyAnsiSlot::Cyan,
            },
            CellColor::Xterm256 { index: 208 },
            CellColor::Truecolor {
                r: 0xff,
                g: 0x00,
                b: 0x80,
            },
        ] {
            let json = serde_json::to_value(color).expect("serialize");
            assert_eq!(
                json["type"],
                serde_json::json!(match color {
                    CellColor::Ansi16 { .. } => "ansi16",
                    CellColor::Xterm256 { .. } => "xterm256",
                    CellColor::Truecolor { .. } => "truecolor",
                })
            );
            let back: CellColor = serde_json::from_value(json).expect("deserialize");
            assert_eq!(back, color);
        }
    }

    #[test]
    fn cell_style_attributes_are_always_present_never_optional() {
        // `CellStyle` is what the wire's `styles` tables carry, so the
        // always-present-boolean discipline is now pinned on it directly.
        let json = serde_json::to_value(style()).expect("serialize");
        let object = json.as_object().expect("object");
        for attr in [
            "bold",
            "dim",
            "italic",
            "blink",
            "inverse",
            "invisible",
            "strikethrough",
            "overline",
        ] {
            assert!(object.contains_key(attr), "missing {attr}");
            assert_eq!(object[attr], serde_json::json!(false));
        }
        // `underline` is deliberately NOT in that list. It was an
        // always-present boolean until it had to carry a shape, and a cell is
        // un-underlined or one of five shapes — six states, which no boolean
        // holds. It is an omitted optional now, like the colors.
        //
        // Absent optionals are OMITTED, not null (the tri-state discipline).
        assert!(!object.contains_key("underline"));
        assert!(!object.contains_key("fg"));
        assert!(!object.contains_key("bg"));
        assert!(!object.contains_key("underline_color"));
    }

    #[test]
    fn underline_shape_survives_the_wire() {
        // The regression the underline widening exists to prevent: a curly
        // underline reported as indistinguishable from a plain one. Under the
        // previous `bool` both of these serialized to `"underline": true`.
        let mut curly = style();
        curly.underline = Some(CellUnderline::Curly);
        let mut single = style();
        single.underline = Some(CellUnderline::Single);

        let curly_json = serde_json::to_value(&curly).expect("serialize");
        let single_json = serde_json::to_value(&single).expect("serialize");
        assert_ne!(
            curly_json, single_json,
            "two different underline shapes serialized identically"
        );

        let back: CellStyle = serde_json::from_value(curly_json).expect("deserialize");
        assert_eq!(back.underline, Some(CellUnderline::Curly));
    }

    #[test]
    fn underline_color_is_independent_of_the_foreground() {
        // SGR 58 sets the underline's own color; a wire that folded it into
        // `fg` would report a red-underlined white word as a red word.
        let mut styled = style();
        styled.fg = Some(CellColor::Ansi16 {
            slot: PtyAnsiSlot::White,
        });
        styled.underline = Some(CellUnderline::Single);
        styled.underline_color = Some(CellColor::Ansi16 {
            slot: PtyAnsiSlot::Red,
        });

        let json = serde_json::to_value(&styled).expect("serialize");
        let back: CellStyle = serde_json::from_value(json).expect("deserialize");
        assert_eq!(
            back.fg,
            Some(CellColor::Ansi16 {
                slot: PtyAnsiSlot::White
            })
        );
        assert_eq!(
            back.underline_color,
            Some(CellColor::Ansi16 {
                slot: PtyAnsiSlot::Red
            })
        );
    }

    #[test]
    fn glyph_may_be_a_multi_codepoint_grapheme_cluster() {
        // A single Rust `char` could not hold this: family emoji is four
        // scalars joined by ZWJ. The run shape's whole reason for carrying
        // per-cell glyph STRINGS is that this must round-trip intact.
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
        let frame = PtyFrame::from_cells(&[vec![cell(family), cell("x")]]);
        let json = serde_json::to_value(&frame).expect("serialize");
        let back: PtyFrame = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back.cells().expect("expand")[0][0].glyph, family);
    }

    #[test]
    fn empty_glyph_cells_survive_the_wire() {
        // The trailing half of a double-width character is an empty-string
        // glyph styled like its head. A joined-string encoding would lose the
        // cell entirely; the per-cell glyph list is what keeps it.
        let wide = vec![cell("\u{5BEC}"), cell(""), cell("b")];
        let frame = PtyFrame::from_cells(std::slice::from_ref(&wide));
        let json = serde_json::to_value(&frame).expect("serialize");
        let back: PtyFrame = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back.cells().expect("expand")[0], wide);
    }

    #[test]
    fn pty_frame_dimensions_are_derived_not_stored() {
        let frame = PtyFrame::from_cells(&[vec![cell("a"), bold("b")], vec![cell("c"), cell("d")]]);
        let json = serde_json::to_value(&frame).expect("serialize");
        let object = json.as_object().expect("object");
        assert!(!object.contains_key("rows_count"), "no stored geometry");
        assert!(!object.contains_key("cols"), "cols must not be a field");
        let mut keys = object.keys().collect::<Vec<_>>();
        keys.sort();
        assert_eq!(keys, ["rows", "styles"], "exactly the table and the runs");
        let back: PtyFrame = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, frame);
    }

    #[test]
    fn runs_are_tagged_and_round_trip() {
        let cells_run = PtyRun::Cells {
            style: 0,
            glyphs: vec!["h".to_owned(), "i".to_owned()],
        };
        let fill_run = PtyRun::Fill {
            style: 1,
            glyph: " ".to_owned(),
            count: 78,
        };

        let json = serde_json::to_value(&cells_run).expect("serialize");
        assert_eq!(json["type"], "cells");
        assert_eq!(
            serde_json::from_value::<PtyRun>(json).expect("deserialize"),
            cells_run
        );

        let json = serde_json::to_value(&fill_run).expect("serialize");
        assert_eq!(json["type"], "fill");
        assert_eq!(json["count"], 78);
        assert_eq!(
            serde_json::from_value::<PtyRun>(json).expect("deserialize"),
            fill_run
        );

        assert_eq!(cells_run.width(), 2);
        assert_eq!(fill_run.width(), 78);

        // Unknown fields are refused — the same strictness every other wire
        // shape in this contract carries.
        assert!(
            serde_json::from_value::<PtyRun>(serde_json::json!({
                "type": "fill", "style": 0, "glyph": " ", "count": 1, "extra": true
            }))
            .is_err()
        );
    }

    #[test]
    fn from_cells_interns_first_appearance_and_scans_maximal_runs() {
        // Row 0: two blank cells, then two bold — two maximal runs. Row 1:
        // bold first, so the table order is decided by row 0 (first
        // appearance), not by any later reuse.
        let frame = PtyFrame::from_cells(&[
            vec![cell(" "), cell(" "), bold("a"), bold("b")],
            vec![bold("x"), cell("y"), cell("y"), cell("y")],
        ]);

        assert_eq!(frame.styles.len(), 2, "two distinct styles, each once");
        assert!(!frame.styles[0].bold, "blank appeared first");
        assert!(frame.styles[1].bold);

        assert_eq!(
            frame.rows[0],
            vec![
                PtyRun::Fill {
                    style: 0,
                    glyph: " ".to_owned(),
                    count: 2,
                },
                PtyRun::Cells {
                    style: 1,
                    glyphs: vec!["a".to_owned(), "b".to_owned()],
                },
            ],
            "maximal runs: a uniform-glyph run is a fill, a mixed one is cells"
        );
        assert_eq!(
            frame.rows[1],
            vec![
                PtyRun::Fill {
                    style: 1,
                    glyph: "x".to_owned(),
                    count: 1,
                },
                PtyRun::Fill {
                    style: 0,
                    glyph: "y".to_owned(),
                    count: 3,
                },
            ],
            "reused styles reference the table; a single cell is a width-1 fill"
        );
    }

    #[test]
    fn from_cells_then_cells_is_the_identity() {
        let grid = vec![
            vec![cell("g"), cell("w"), bold("!")],
            vec![bold("k"), cell(""), cell(" ")],
        ];
        let frame = PtyFrame::from_cells(&grid);
        assert_eq!(frame.cells().expect("expand"), grid);
    }

    #[test]
    fn expansion_refuses_an_out_of_range_style_index() {
        let frame = PtyFrame {
            styles: vec![style()],
            rows: vec![vec![PtyRun::Cells {
                style: 1,
                glyphs: vec!["x".to_owned()],
            }]],
        };
        assert!(frame.cells().is_none(), "index 1 into a one-entry table");
    }

    #[test]
    fn expansion_refuses_ragged_rows() {
        let frame = PtyFrame {
            styles: vec![style()],
            rows: vec![
                vec![PtyRun::Fill {
                    style: 0,
                    glyph: " ".to_owned(),
                    count: 4,
                }],
                vec![PtyRun::Fill {
                    style: 0,
                    glyph: " ".to_owned(),
                    count: 3,
                }],
            ],
        };
        assert!(frame.cells().is_none(), "rows of width 4 and 3");
    }

    #[test]
    fn expansion_bounds_a_fill_before_allocating_it() {
        // A ~45-byte fill naming four billion cells must be answered by
        // arithmetic, not survived by allocation: the wire's grid universe is
        // u16 x u16 and expansion refuses anything past it.
        let frame = PtyFrame {
            styles: vec![style()],
            rows: vec![vec![PtyRun::Fill {
                style: 0,
                glyph: " ".to_owned(),
                count: u32::MAX,
            }]],
        };
        assert!(frame.cells().is_none());
    }

    #[test]
    fn expansion_tolerates_non_maximal_runs_and_zero_width_runs() {
        // The tolerate ruling: adjacent same-style runs and zero-width runs
        // are non-canonical, not corrupt. A producer should merge; expansion
        // — and every decoder — buys no correctness by refusing here. The
        // kernel's strict decoder refuses the zero-width case with a typed
        // error; at this layer the value still expands to the grid it names.
        let frame = PtyFrame {
            styles: vec![style()],
            rows: vec![vec![
                PtyRun::Cells {
                    style: 0,
                    glyphs: vec!["a".to_owned()],
                },
                PtyRun::Cells {
                    style: 0,
                    glyphs: vec!["b".to_owned()],
                },
                PtyRun::Fill {
                    style: 0,
                    glyph: " ".to_owned(),
                    count: 0,
                },
            ]],
        };
        let cells = frame.cells().expect("tolerated");
        assert_eq!(cells[0].len(), 2);
        assert_eq!(cells[0][0].glyph, "a");
        assert_eq!(cells[0][1].glyph, "b");
    }

    #[test]
    fn pty_delta_kinds_are_tagged_and_the_batch_table_is_self_contained() {
        let cells_changed = PtyDelta::CellsChanged {
            styles: vec![style()],
            updates: vec![PtyCellUpdate {
                row: 0,
                col: 0,
                glyph: "x".to_owned(),
                style: 0,
            }],
        };
        let resized = PtyDelta::Resized {
            rows: 40,
            cols: 120,
        };

        let json = serde_json::to_value(&cells_changed).expect("serialize");
        assert_eq!(json["type"], "cells_changed");
        assert_eq!(
            json["styles"].as_array().expect("table").len(),
            1,
            "the batch carries its own style table"
        );
        assert_eq!(json["updates"][0]["style"], 0);
        assert_eq!(
            serde_json::from_value::<PtyDelta>(json).expect("deserialize"),
            cells_changed
        );

        let json = serde_json::to_value(&resized).expect("serialize");
        assert_eq!(json["type"], "resized");
        assert_eq!(json["rows"], 40);
        assert_eq!(json["cols"], 120);
        assert!(json.get("updates").is_none());
        assert!(json.get("styles").is_none());
        assert_eq!(
            serde_json::from_value::<PtyDelta>(json).expect("deserialize"),
            resized
        );
    }
}
