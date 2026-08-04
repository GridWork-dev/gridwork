//! Typed cell-style attributes: what a delta carries about a cell besides its
//! glyph.
//!
//! [`crate::render::Row`] already carries `text`; [`CellStyle`] is the same
//! kind of typed fact about the same cells, sourced the same way — read off
//! libghostty-vt's own per-cell style query, never inferred from the bytes a
//! consumer would otherwise have to re-parse out of a VT dump.

use libghostty_vt::style::{Style as VtStyle, StyleColor, Underline as VtUnderline};

/// A color set on one of a cell's attributes (foreground, background, or
/// underline).
///
/// Deliberately not resolved through a palette to a final RGB triplet: the
/// SGR form a child chose is part of the terminal's state, and collapsing it
/// to one pixel value would throw that away before a consumer ever sees it.
/// Resolving a [`Color::Palette`] index against an actual palette — which
/// palette, and how a theme maps it to a pixel — is a rendering decision, and
/// this crate renders nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// A palette index (0-255).
    ///
    // Derivation: ECMA-48 §8.3.117 — SGR parameters 30-37/40-47 name the eight
    // basic display/background colours as bare parameter values, unqualified
    // by any colour space, which is what makes them palette lookups rather
    // than direct values. XTERM-CTLSEQS "Character Attributes (SGR)" extends
    // the same shape twice — the aixterm 90-97/100-107 range for the bright
    // eight, and the `38;5;Ps`/`48;5;Ps` form for the full 256-entry table —
    // and all three select a SLOT, not a colour, so this variant carries the
    // index rather than resolving it.
    Palette(u8),
    /// A direct 24-bit color.
    ///
    // Derivation: XTERM-CTLSEQS "Character Attributes (SGR)" — the
    // `38;2;r;g;b`/`48;2;r;g;b` form carries an RGB triplet directly; no
    // palette is consulted, which is the fact that makes this a separate
    // variant from `Palette` rather than a lookup result rendered the same way.
    Rgb(u8, u8, u8),
}

/// How a cell's text is underlined, when it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Underline {
    // Derivation: ECMA-48 §8.3.117 — SGR parameter 4 is "singly underlined".
    /// One line (`CSI 4 m`).
    Single,
    // Derivation: ECMA-48 §8.3.117 — SGR parameter 21 is "doubly underlined",
    // a real ECMA-48 rendition and not this crate's own extension — some
    // terminals instead treat 21 as "bold off", but that reading is not what
    // the spec says, and it is not what the engine underneath this crate does
    // either (asserted in `tests/style.rs`).
    /// Two parallel lines (`CSI 21 m`).
    Double,
    // Derivation: KITTY-UNDERLINES — the colon-separated style sub-parameter
    // on SGR 4 selects a shape beyond ECMA-48's single/double: `4:3` curly,
    // `4:4` dotted, `4:5` dashed. All three are the same mechanism (a
    // sub-parameter on the same control function) and this crate maps them
    // uniformly; the fidelity suite exercises `4:3` as the representative case.
    /// A wavy line (`CSI 4:3 m`).
    Curly,
    /// A dotted line (`CSI 4:4 m`).
    Dotted,
    /// A dashed line (`CSI 4:5 m`).
    Dashed,
}

/// Typed visual attributes of one cell, independent of its glyph.
///
/// The default value is the SGR-0 rendition: no colors, every flag off — see
/// `sgr_0_resets_every_attribute` in `tests/style.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellStyle {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    // Derivation: KITTY-UNDERLINES — SGR 58/59 set the underline's own color,
    // independent of the text's foreground, "identical to the codes used for
    // setting foreground(38)/background(48) colors". ECMA-48 §8.3.117 lists
    // 58/59 as reserved and assigns them nothing, so the extension — not the
    // base spec — is what makes this a real, separate attribute.
    pub underline_color: Option<Color>,
    // Derivation: ECMA-48 §8.3.117 — SGR 1 is "bold or increased intensity".
    pub bold: bool,
    // Derivation: ECMA-48 §8.3.117 — SGR 2 is "faint, decreased intensity or
    // second colour". Named `dim` here, not `faint`: this crate's own word for
    // the same attribute libghostty-vt calls `faint`.
    pub dim: bool,
    // Derivation: ECMA-48 §8.3.117 — SGR 3 is "italicized".
    pub italic: bool,
    pub underline: Option<Underline>,
    // Derivation: ECMA-48 §8.3.117 — SGR 5 and 6 are the slow and rapid blink
    // rates; this crate does not distinguish them, matching the single
    // boolean libghostty-vt itself already collapses both into.
    pub blink: bool,
    // Derivation: ECMA-48 §8.3.117 — SGR 7 is "negative image".
    pub inverse: bool,
    // Derivation: ECMA-48 §8.3.117 — SGR 8 is "concealed characters".
    pub invisible: bool,
    // Derivation: ECMA-48 §8.3.117 — SGR 9 is "crossed-out".
    pub strikethrough: bool,
    // Derivation: ECMA-48 §8.3.117 — SGR 53 is "overlined".
    pub overline: bool,
}

impl From<VtStyle> for CellStyle {
    fn from(style: VtStyle) -> Self {
        Self {
            fg: color_from(style.fg_color),
            bg: color_from(style.bg_color),
            underline_color: color_from(style.underline_color),
            bold: style.bold,
            dim: style.faint,
            italic: style.italic,
            underline: underline_from(style.underline),
            blink: style.blink,
            inverse: style.inverse,
            invisible: style.invisible,
            strikethrough: style.strikethrough,
            overline: style.overline,
        }
    }
}

fn color_from(color: StyleColor) -> Option<Color> {
    match color {
        StyleColor::None => None,
        StyleColor::Palette(index) => Some(Color::Palette(index.0)),
        StyleColor::Rgb(rgb) => Some(Color::Rgb(rgb.r, rgb.g, rgb.b)),
    }
}

fn underline_from(underline: VtUnderline) -> Option<Underline> {
    match underline {
        VtUnderline::None => None,
        VtUnderline::Single => Some(Underline::Single),
        VtUnderline::Double => Some(Underline::Double),
        VtUnderline::Curly => Some(Underline::Curly),
        VtUnderline::Dotted => Some(Underline::Dotted),
        VtUnderline::Dashed => Some(Underline::Dashed),
        // ponytail: `VtUnderline` is `#[non_exhaustive]` upstream, so this arm
        // is required even though every variant it currently defines is
        // matched above. A future variant reads as no underline rather than
        // failing the whole cell's style — the same fallback shape this
        // crate already uses for a libghostty-vt error elsewhere (`.ok()?`).
        _ => None,
    }
}
