//! Fidelity of the typed style a delta carries alongside its glyphs.
//!
//! These are API tests against [`Renderer::frame`], not a VT round-trip: each
//! one writes a short SGR sequence, takes the resulting [`Row::styles`], and
//! asserts the typed [`CellStyle`] libghostty-vt reports for it — never bytes
//! compared against bytes. Where a `Derivation:` marker below cites a SGR
//! parameter's meaning, that citation is what makes the assertion a fidelity
//! check rather than a guess at what the engine happens to do.

use gwk_pty::render::Row;
use gwk_pty::{CellStyle, Color, Grid, Renderer, Underline};

/// The per-cell styles of row 0 after writing `input` to a fresh grid.
///
/// Two columns is enough for every case below: column 0 carries the SGR
/// under test, column 1 (where used) carries whatever a following reset or
/// off-code left behind.
fn styles_of(input: &[u8]) -> Vec<CellStyle> {
    let mut grid = Grid::new(10, 1).expect("10x1 grid");
    let mut renderer = Renderer::new().expect("render state");
    grid.write(input);
    let frame = renderer.frame(&grid, 0).expect("frame");
    let row: &Row = frame
        .changed
        .iter()
        .find(|r| r.y == 0)
        .expect("row 0 changed");
    row.styles.clone()
}

// --- Attributes -------------------------------------------------------

#[test]
fn bold_sets_the_bold_flag() {
    // Derivation: ECMA-48 §8.3.117 — SGR 1 is "bold or increased intensity".
    let styles = styles_of(b"\x1b[1mX");
    assert!(styles[0].bold, "SGR 1 should set bold: {:?}", styles[0]);
}

#[test]
fn dim_sets_the_dim_flag() {
    // Derivation: ECMA-48 §8.3.117 — SGR 2 is "faint, decreased intensity or
    // second colour".
    let styles = styles_of(b"\x1b[2mX");
    assert!(styles[0].dim, "SGR 2 should set dim: {:?}", styles[0]);
}

#[test]
fn italic_sets_the_italic_flag() {
    // Derivation: ECMA-48 §8.3.117 — SGR 3 is "italicized".
    let styles = styles_of(b"\x1b[3mX");
    assert!(styles[0].italic, "SGR 3 should set italic: {:?}", styles[0]);
}

#[test]
fn underline_single_is_the_default_underline() {
    // Derivation: ECMA-48 §8.3.117 — SGR 4 is "singly underlined".
    let styles = styles_of(b"\x1b[4mX");
    assert_eq!(styles[0].underline, Some(Underline::Single));
}

#[test]
fn underline_double_is_a_real_ecma_48_rendition() {
    // Derivation: ECMA-48 §8.3.117 — SGR 21 is "doubly underlined". Some
    // terminals instead treat 21 as "bold off" (a non-standard reading), so
    // this pins the standard one against a bold cell: bold must survive.
    let styles = styles_of(b"\x1b[1;21mX");
    assert_eq!(styles[0].underline, Some(Underline::Double));
    assert!(
        styles[0].bold,
        "SGR 21 is double underline, not bold-off: {:?}",
        styles[0]
    );
}

#[test]
fn underline_curly_is_the_colon_subparameter_form() {
    // Derivation: KITTY-UNDERLINES — the colon-separated sub-parameter `4:3`
    // selects a curly underline, a shape beyond ECMA-48's single/double.
    let styles = styles_of(b"\x1b[4:3mX");
    assert_eq!(styles[0].underline, Some(Underline::Curly));
}

#[test]
fn inverse_sets_the_inverse_flag() {
    // Derivation: ECMA-48 §8.3.117 — SGR 7 is "negative image".
    let styles = styles_of(b"\x1b[7mX");
    assert!(
        styles[0].inverse,
        "SGR 7 should set inverse: {:?}",
        styles[0]
    );
}

#[test]
fn strikethrough_sets_the_strikethrough_flag() {
    // Derivation: ECMA-48 §8.3.117 — SGR 9 is "crossed-out".
    let styles = styles_of(b"\x1b[9mX");
    assert!(
        styles[0].strikethrough,
        "SGR 9 should set strikethrough: {:?}",
        styles[0]
    );
}

// --- Color: 16, 256, and truecolor, foreground and background ---------

#[test]
fn foreground_16_color_is_a_palette_index() {
    // Derivation: ECMA-48 §8.3.117 — SGR 31 is "red display", one of the
    // eight basic named colours. XTERM-CTLSEQS "Character Attributes (SGR)"
    // extends the aixterm 90-97 range as the bright eight. Both are palette
    // selectors, tested together so the two ranges are proven distinct
    // indices rather than aliases of each other.
    assert_eq!(
        styles_of(b"\x1b[31mX")[0].fg,
        Some(Color::Palette(1)),
        "basic red (SGR 31)"
    );
    assert_eq!(
        styles_of(b"\x1b[91mX")[0].fg,
        Some(Color::Palette(9)),
        "bright red (SGR 91)"
    );
}

#[test]
fn background_16_color_is_a_palette_index() {
    // Derivation: ECMA-48 §8.3.117 — SGR 44 is "blue background".
    assert_eq!(styles_of(b"\x1b[44mX")[0].bg, Some(Color::Palette(4)));
}

#[test]
fn foreground_256_color_is_a_palette_index() {
    // Derivation: XTERM-CTLSEQS "Character Attributes (SGR)" — `38;5;Ps`
    // selects a foreground colour by index into the full 256-entry table.
    // 208 is outside the basic-and-bright 0-15 range, so this cannot pass by
    // accident of the 16-color path above landing on the same index.
    assert_eq!(
        styles_of(b"\x1b[38;5;208mX")[0].fg,
        Some(Color::Palette(208))
    );
}

#[test]
fn background_256_color_is_a_palette_index() {
    // Derivation: XTERM-CTLSEQS "Character Attributes (SGR)" — `48;5;Ps`
    // is the background counterpart of `38;5;Ps`.
    assert_eq!(styles_of(b"\x1b[48;5;99mX")[0].bg, Some(Color::Palette(99)));
}

#[test]
fn foreground_truecolor_is_an_rgb_triplet() {
    // Derivation: XTERM-CTLSEQS "Character Attributes (SGR)" — `38;2;r;g;b`
    // sets the foreground directly from an RGB triplet; no palette entry is
    // involved, which is what makes this a `Color::Rgb` and not a
    // `Color::Palette` of whichever index happens to be closest.
    assert_eq!(
        styles_of(b"\x1b[38;2;10;20;30mX")[0].fg,
        Some(Color::Rgb(10, 20, 30))
    );
}

#[test]
fn background_truecolor_is_an_rgb_triplet() {
    // Derivation: XTERM-CTLSEQS "Character Attributes (SGR)" — `48;2;r;g;b`
    // is the background counterpart of `38;2;r;g;b`.
    assert_eq!(
        styles_of(b"\x1b[48;2;40;50;60mX")[0].bg,
        Some(Color::Rgb(40, 50, 60))
    );
}

// --- Reset semantics ----------------------------------------------------

#[test]
fn sgr_0_resets_every_attribute() {
    // Derivation: ECMA-48 §8.3.117 — SGR 0 is "default rendition
    // (implementation-defined), cancels the effect of any preceding
    // occurrence of SGR in the data stream". A maximally-styled cell first,
    // so this proves 0 clears everything rather than merely the one or two
    // fields a smaller setup would happen to exercise.
    let styles = styles_of(b"\x1b[1;3;4;7;9;31;44mX\x1b[0mY");
    assert!(styles[0].bold, "setup: first cell should be styled");
    assert_eq!(
        styles[1],
        CellStyle::default(),
        "SGR 0 should clear every attribute: {:?}",
        styles[1]
    );
}

#[test]
fn bold_off_22_clears_only_bold() {
    // Derivation: ECMA-48 §8.3.117 — SGR 22 is "normal colour or normal
    // intensity (neither bold nor faint)".
    let styles = styles_of(b"\x1b[1mX\x1b[22mY");
    assert_eq!(styles[1], CellStyle::default());
}

#[test]
fn italic_off_23_clears_only_italic() {
    // Derivation: ECMA-48 §8.3.117 — SGR 23 is "not italicized, not fraktur".
    let styles = styles_of(b"\x1b[3mX\x1b[23mY");
    assert_eq!(styles[1], CellStyle::default());
}

#[test]
fn underline_off_24_clears_underline() {
    // Derivation: ECMA-48 §8.3.117 — SGR 24 is "not underlined (neither
    // singly nor doubly)".
    let styles = styles_of(b"\x1b[4mX\x1b[24mY");
    assert_eq!(styles[1], CellStyle::default());
}

#[test]
fn inverse_off_27_clears_inverse() {
    // Derivation: ECMA-48 §8.3.117 — SGR 27 is "positive image".
    let styles = styles_of(b"\x1b[7mX\x1b[27mY");
    assert_eq!(styles[1], CellStyle::default());
}

#[test]
fn strikethrough_off_29_clears_strikethrough() {
    // Derivation: ECMA-48 §8.3.117 — SGR 29 is "not crossed out".
    let styles = styles_of(b"\x1b[9mX\x1b[29mY");
    assert_eq!(styles[1], CellStyle::default());
}

#[test]
fn foreground_default_39_clears_foreground() {
    // Derivation: ECMA-48 §8.3.117 — SGR 39 is "default display colour
    // (implementation-defined)".
    let styles = styles_of(b"\x1b[31mX\x1b[39mY");
    assert_eq!(styles[1].fg, None);
}

#[test]
fn background_default_49_clears_background() {
    // Derivation: ECMA-48 §8.3.117 — SGR 49 is "default background colour
    // (implementation-defined)".
    let styles = styles_of(b"\x1b[44mX\x1b[49mY");
    assert_eq!(styles[1].bg, None);
}

// --- Seeded negative ------------------------------------------------------

#[test]
fn a_dropped_attribute_fails_here_not_silently() {
    // The tests above each isolate one attribute, which means each one would
    // still pass even if `CellStyle::from` silently dropped a FIELD none of
    // them individually looks at — `overline`, `blink`, `invisible`, and
    // `underline_color` are never set by any test above, so a conversion
    // that forgot to wire one up would sail through every one of them. This
    // is the seeded-negative check: set every attribute at once and assert
    // the WHOLE struct against a fully-specified literal, so dropping any
    // single one fails right here. Every field of the literal is in its
    // non-default state — a `false`/`None` anywhere would put that field
    // back outside the check's reach.
    //
    // Derivation: ECMA-48 §8.3.117 — SGR 53 is "overlined", SGR 5 is
    // "slowly blinking", SGR 8 is "concealed characters"; SGR is defined
    // with cumulative effect, which is what lets the attribute pile below
    // arrive as several sequences instead of one over-budget parameter
    // list.
    // Derivation: KITTY-UNDERLINES — SGR 58 with the colon-separated `2`
    // sub-parameter form sets the underline's own direct-RGB color.
    let styles = styles_of(
        b"\x1b[1;2;3;4;5;7;8;9;53m\x1b[38;2;10;20;30m\x1b[48;2;40;50;60m\x1b[58:2::70:80:90mX",
    );
    assert_eq!(
        styles[0],
        CellStyle {
            fg: Some(Color::Rgb(10, 20, 30)),
            bg: Some(Color::Rgb(40, 50, 60)),
            underline_color: Some(Color::Rgb(70, 80, 90)),
            bold: true,
            dim: true,
            italic: true,
            underline: Some(Underline::Single),
            blink: true,
            inverse: true,
            invisible: true,
            strikethrough: true,
            overline: true,
        },
        "one attribute did not survive the conversion: {:?}",
        styles[0]
    );
}

// --- Cluster segmentation vs `text` ---------------------------------------

// A ZWJ emoji sequence is ONE UAX-29 grapheme cluster of `text` but, with
// grapheme-cluster mode reset (the default), MORE THAN ONE cell — so more
// than one `styles` entry. These two tests pin both sides of the condition
// documented on `Row::styles`: a consumer walking `text` with UAX-29 reads
// aligned styles only under mode 2027.

#[test]
fn a_zwj_sequence_spans_two_style_entries_by_default() {
    // Derivation: UAX-29 — an extended grapheme cluster does not break
    // before or after ZERO WIDTH JOINER (rules GB9/GB11), so WOMAN + ZWJ +
    // ROCKET segments as one grapheme of `text`.
    // Derivation: TERM-UNICODE-CORE — with mode 2027 reset a terminal uses
    // legacy width-based segmentation, under which the joined sequence
    // occupies separate cells.
    let styles = styles_of("\x1b[31m\u{1F469}\u{200D}\u{1F680}".as_bytes());
    assert_eq!(styles[0].fg, Some(Color::Palette(1)));
    assert_eq!(
        styles[1].fg,
        Some(Color::Palette(1)),
        "the second half of the split cluster carries its own entry"
    );
}

#[test]
fn mode_2027_makes_a_zwj_sequence_one_entry() {
    // Derivation: TERM-UNICODE-CORE — under DECSET 2027 a cell holds an
    // extended grapheme cluster, so the engine's segmentation and a UAX-29
    // walk of `text` agree and `styles[i]` IS the i-th grapheme's style.
    let styles = styles_of("\x1b[?2027h\x1b[31m\u{1F469}\u{200D}\u{1F680}".as_bytes());
    assert_eq!(styles[0].fg, Some(Color::Palette(1)));
    assert_eq!(styles[1].fg, None, "one cluster, one styled entry");
}
