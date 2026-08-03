//! The one place a token becomes a renderer colour.
//!
//! `gwk-theme` resolves a token to a slot NUMBER and stops. This is where that
//! number becomes a `ratatui::style::Color`, and it is the only place it does:
//! neither ratatui nor crossterm degrades colour on its own — crossterm emits
//! `38;2;r;g;b` verbatim whatever the terminal can take, and ratatui documents
//! the result on a non-truecolor terminal as literally "unpredictable". The
//! whole degradation path is ours, which is exactly why it lives behind one
//! function instead of a capability check per widget.

use gwk_theme::marks::{Mark, StateBinding};
use gwk_theme::{AnsiSlot, ColorTier, Paint, Token};
use ratatui::style::{Color, Modifier, Style};

/// A slot as the renderer names it.
///
/// The renderer's palette names are NOT the ANSI names, and the three places
/// they disagree are the three places to be exactly one slot off: `Gray` is
/// slot 7 (ANSI white), `DarkGray` is slot 8 (ANSI bright black), and `White`
/// is slot 15 (ANSI bright white).
pub const fn color(slot: AnsiSlot) -> Color {
    match slot {
        AnsiSlot::Black => Color::Black,
        AnsiSlot::Red => Color::Red,
        AnsiSlot::Green => Color::Green,
        AnsiSlot::Yellow => Color::Yellow,
        AnsiSlot::Blue => Color::Blue,
        AnsiSlot::Magenta => Color::Magenta,
        AnsiSlot::Cyan => Color::Cyan,
        AnsiSlot::White => Color::Gray,
        AnsiSlot::BrightBlack => Color::DarkGray,
        AnsiSlot::BrightRed => Color::LightRed,
        AnsiSlot::BrightGreen => Color::LightGreen,
        AnsiSlot::BrightYellow => Color::LightYellow,
        AnsiSlot::BrightBlue => Color::LightBlue,
        AnsiSlot::BrightMagenta => Color::LightMagenta,
        AnsiSlot::BrightCyan => Color::LightCyan,
        AnsiSlot::BrightWhite => Color::White,
    }
}

/// A resolved paint as a style.
///
/// An unpainted token yields the DEFAULT style — no foreground set at all,
/// rather than a foreground set to something invisible. That is the difference
/// between a token that has no expression at this tier and a token painted the
/// same colour as the background, and only the first one is honest.
pub fn paint_style(paint: Paint) -> Style {
    match paint {
        Paint::Rgb(r, g, b) => Style::default().fg(Color::Rgb(r, g, b)),
        Paint::Indexed(index) => Style::default().fg(Color::Indexed(index)),
        Paint::Slot(slot) => Style::default().fg(color(slot)),
        Paint::BoldSlot(slot) => Style::default()
            .fg(color(slot))
            .add_modifier(Modifier::BOLD),
        Paint::Reset => Style::default().fg(Color::Reset),
        Paint::ReverseVideo => Style::default().add_modifier(Modifier::REVERSED),
        Paint::Unpainted => Style::default(),
    }
}

/// The style for a token at a tier.
pub fn token_style(token: &Token, tier: ColorTier) -> Style {
    paint_style(token.paint(tier))
}

/// The style for a state's mark at a tier.
pub fn state_style(state: &StateBinding, tier: ColorTier) -> Style {
    match gwk_theme::SIGNAL.iter().find(|t| t.name == state.token) {
        Some(token) => token_style(token, tier),
        // Unreachable — the binding tables pin every reference. An unstyled
        // cell is the safe answer: the glyph still carries the state, which is
        // the property the whole surface is built on.
        None => Style::default(),
    }
}

/// The glyph a mark shows on `frame`, honouring the escape.
///
/// A cycling mark advances; a static one ignores the frame counter. Under the
/// escape every mark is its single ASCII fallback, cycles included — an ASCII
/// spinner that animated would need eight ASCII frames nobody ruled.
pub fn glyph(mark: &Mark, frame: usize, ascii: bool) -> char {
    if ascii {
        return mark.ascii;
    }
    if mark.glyphs.is_empty() {
        return mark.ascii;
    }
    mark.glyphs[frame % mark.glyphs.len()]
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use gwk_theme::SIGNAL;
    use gwk_theme::marks::{MARKS, STATES, mark};

    use super::*;

    fn token(name: &str) -> &'static Token {
        SIGNAL
            .iter()
            .find(|token| token.name == name)
            .unwrap_or_else(|| panic!("no token {name:?}"))
    }

    #[test]
    fn tier_16_slots_map_onto_the_renderers_shifted_names() {
        assert_eq!(color(AnsiSlot::White), Color::Gray);
        assert_eq!(color(AnsiSlot::BrightBlack), Color::DarkGray);
        assert_eq!(color(AnsiSlot::BrightWhite), Color::White);
        // And no two slots collapse onto one colour, which is the failure the
        // three above are the likely cause of.
        let mapped: BTreeSet<String> = AnsiSlot::ALL
            .iter()
            .map(|slot| format!("{:?}", color(*slot)))
            .collect();
        assert_eq!(mapped.len(), 16);
    }

    #[test]
    fn tier_truecolor_and_256_carry_colour_and_mono_carries_none() {
        assert_eq!(
            token_style(token("fail"), ColorTier::Truecolor),
            Style::default().fg(Color::Rgb(0xFF, 0x6E, 0x6E))
        );
        assert_eq!(
            token_style(token("fail"), ColorTier::Xterm256),
            Style::default().fg(Color::Indexed(203))
        );
        assert_eq!(
            token_style(token("fail"), ColorTier::Ansi16),
            Style::default().fg(Color::LightRed)
        );
        assert_eq!(
            token_style(token("fail"), ColorTier::Mono),
            Style::default().fg(Color::Reset)
        );
    }

    #[test]
    fn tier_bold_is_the_escalation_channel_at_sixteen_only() {
        let bright = token("hue_bright");
        let at16 = token_style(bright, ColorTier::Ansi16);
        assert_eq!(at16.fg, Some(Color::LightCyan));
        assert!(at16.add_modifier.contains(Modifier::BOLD));
        // `hue` and `hue_bright` share slot 14; weight is what keeps them
        // apart once the palette is gone.
        assert_eq!(
            token_style(token("hue"), ColorTier::Ansi16).fg,
            Some(Color::LightCyan)
        );
        assert!(
            !token_style(token("hue"), ColorTier::Ansi16)
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            !token_style(bright, ColorTier::Mono)
                .add_modifier
                .contains(Modifier::BOLD),
            "no ratified row assigns weight at mono"
        );
    }

    #[test]
    fn tier_reverse_video_is_an_attribute_and_never_a_colour() {
        for tier in ColorTier::ALL
            .iter()
            .filter(|t| matches!(t, ColorTier::Ansi16) || matches!(t, ColorTier::Mono))
        {
            for name in ["focus", "selection"] {
                let style = token_style(token(name), *tier);
                assert_eq!(style.fg, None, "{name} at {}", tier.as_str());
                assert!(
                    style.add_modifier.contains(Modifier::REVERSED),
                    "{name} at {}",
                    tier.as_str()
                );
            }
        }
    }

    #[test]
    fn tier_an_unpainted_token_sets_no_foreground_at_all() {
        for tier in ColorTier::ALL {
            for name in ["bg", "surface", "surface_2"] {
                assert_eq!(token_style(token(name), *tier), Style::default());
            }
            assert_eq!(
                token_style(token("faint"), *tier) == Style::default(),
                matches!(tier, ColorTier::Ansi16 | ColorTier::Mono),
                "faint at {}",
                tier.as_str()
            );
        }
    }

    #[test]
    fn tier_every_state_resolves_to_a_style_at_every_tier() {
        for tier in ColorTier::ALL {
            for state in STATES {
                // Not `Style::default()`: every state binds to a token that has
                // a colour somewhere, so an unstyled result at truecolor would
                // mean a binding fell through.
                if matches!(tier, ColorTier::Truecolor) {
                    assert!(
                        state_style(state, *tier).fg.is_some(),
                        "state {:?} has no colour at truecolor",
                        state.name
                    );
                }
            }
        }
    }

    #[test]
    fn a_cycling_mark_advances_and_a_static_one_does_not() {
        let spinner = mark("spinner").expect("spinner");
        let frames: BTreeSet<char> = (0..8).map(|f| glyph(spinner, f, false)).collect();
        assert_eq!(frames.len(), 8);
        assert_eq!(glyph(spinner, 8, false), glyph(spinner, 0, false));

        let done = mark("done").expect("done");
        assert_eq!(glyph(done, 0, false), glyph(done, 7, false));

        // The escape flattens the cycle: one mark, one ASCII character.
        for entry in MARKS {
            assert_eq!(glyph(entry, 0, true), entry.ascii);
            assert_eq!(glyph(entry, 5, true), entry.ascii);
        }
    }
}
