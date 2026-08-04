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
//! [`PtyAnsiSlot`] duplicates `gwk_theme::tier::AnsiSlot`'s sixteen names
//! deliberately rather than sharing it: that type is a THEME token's
//! rendering disposition, this one is the raw ANSI slot a wire cell reports.
//! The two will keep agreeing on names because both describe the same
//! sixteen terminal slots, but they answer different questions and a shared
//! dependency would couple them for no reason — and specta's TypeScript
//! export refuses two registered types that share a bare name, so a shared
//! name here would not even compile once both crates reach `bindings.ts`.

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
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

/// The attribute bits and colors a [`StyledCell`] carries, independent of its
/// glyph. The six attributes are plain, always-present booleans — not
/// `Option<bool>` — matching how this contract already carries `dirty` and
/// `unpushed` elsewhere (`docs/contract/NAMING.md`): a boolean fact has no
/// third "absent" state to distinguish from `false`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct CellStyle {
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub strikethrough: bool,
    /// Absent means the terminal's own default foreground.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub fg: Option<CellColor>,
    /// Absent means the terminal's own default background.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub bg: Option<CellColor>,
}

/// One terminal cell: a displayed glyph plus the style it carries.
///
/// `glyph` is a `String`, not a `char` — a cell can hold a full grapheme
/// cluster (a combining mark, a ZWJ emoji sequence), which a single Rust
/// `char` cannot represent. An empty string is a legal glyph: a blank cell,
/// or the trailing half of a double-width character. Column width — a wide
/// glyph occupies two grid cells — is deliberately NOT modeled here: deciding
/// it is part of the engine-side conversion this task defers.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct StyledCell {
    pub glyph: String,
    pub style: CellStyle,
}

/// A full styled frame: every cell of a hosted PTY session's grid at one
/// [`crate::ids::PtyFrameSeq`], row-major (`cells[row][col]`).
///
/// `rows`/`cols` are not stored as separate fields — they are exactly
/// `cells.len()` and `cells[0].len()`, every row is the same length, and this
/// type has no constructor that could let a stored pair disagree with the
/// grid it describes. That shape rule is the kernel wire layer's to enforce
/// (see `crate::protocol`'s module doc on the split between what a type
/// admits and what the strict decoder refuses), not this type's to
/// self-validate: a wire type validates itself only when its value set is
/// CLOSED, and a rectangular grid of arbitrary size is not.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct PtyFrame {
    pub cells: Vec<Vec<StyledCell>>,
}

/// One cell's new content, addressed by zero-indexed grid position.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct PtyCellUpdate {
    pub row: u16,
    pub col: u16,
    pub cell: StyledCell,
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
    /// the grid's CURRENT size — the most recent snapshot or `resized` delta.
    CellsChanged { updates: Vec<PtyCellUpdate> },
    /// The session's grid was resized. Cell content outside the new bounds is
    /// gone; a client that needs the new content re-requests
    /// [`crate::protocol::KernelRequest::PtySnapshot`].
    Resized { rows: u16, cols: u16 },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(glyph: &str) -> StyledCell {
        StyledCell {
            glyph: glyph.to_owned(),
            style: CellStyle {
                bold: false,
                dim: false,
                italic: false,
                underline: false,
                inverse: false,
                strikethrough: false,
                fg: None,
                bg: None,
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
    fn styled_cell_attributes_are_always_present_never_optional() {
        let json = serde_json::to_value(cell("x")).expect("serialize");
        let object = json.as_object().expect("object");
        let style = object["style"].as_object().expect("style object");
        for attr in [
            "bold",
            "dim",
            "italic",
            "underline",
            "inverse",
            "strikethrough",
        ] {
            assert!(style.contains_key(attr), "missing {attr}");
            assert_eq!(style[attr], serde_json::json!(false));
        }
        // Absent optionals are OMITTED, not null (the tri-state discipline).
        assert!(!style.contains_key("fg"));
        assert!(!style.contains_key("bg"));
    }

    #[test]
    fn glyph_may_be_a_multi_codepoint_grapheme_cluster() {
        // A single Rust `char` could not hold this: family emoji is four
        // scalars joined by ZWJ. The wire shape's whole reason for using
        // `String` instead of `char` is that this must round-trip intact.
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
        let styled = cell(family);
        let json = serde_json::to_value(&styled).expect("serialize");
        let back: StyledCell = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back.glyph, family);
    }

    #[test]
    fn pty_frame_dimensions_are_derived_not_stored() {
        let frame = PtyFrame {
            cells: vec![vec![cell("a"), cell("b")], vec![cell("c"), cell("d")]],
        };
        let json = serde_json::to_value(&frame).expect("serialize");
        let object = json.as_object().expect("object");
        assert!(!object.contains_key("rows"), "rows must not be a field");
        assert!(!object.contains_key("cols"), "cols must not be a field");
        assert_eq!(frame.cells.len(), 2);
        assert_eq!(frame.cells[0].len(), 2);
        let back: PtyFrame = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, frame);
    }

    #[test]
    fn pty_delta_kinds_are_tagged_and_resize_carries_no_cell_coordinates() {
        let cells_changed = PtyDelta::CellsChanged {
            updates: vec![PtyCellUpdate {
                row: 0,
                col: 0,
                cell: cell("x"),
            }],
        };
        let resized = PtyDelta::Resized {
            rows: 40,
            cols: 120,
        };

        let json = serde_json::to_value(&cells_changed).expect("serialize");
        assert_eq!(json["type"], "cells_changed");
        assert_eq!(
            serde_json::from_value::<PtyDelta>(json).expect("deserialize"),
            cells_changed
        );

        let json = serde_json::to_value(&resized).expect("serialize");
        assert_eq!(json["type"], "resized");
        assert_eq!(json["rows"], 40);
        assert_eq!(json["cols"], 120);
        assert!(json.get("updates").is_none());
        assert_eq!(
            serde_json::from_value::<PtyDelta>(json).expect("deserialize"),
            resized
        );
    }
}
