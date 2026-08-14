//! The ratified SIGNAL vocabulary — data and total functions, no rendering, no
//! dependencies beyond serialization.
//!
//! One source of truth consumed three ways: the site's CSS custom properties
//! (checked by `tools/check-theme-sync.sh` — token `name` maps to
//! `--kebab-case`), the TUI theme, and the generated TypeScript contract.
//!
//! Every name carries the [`CSS_PREFIX`] namespace, because one of those three
//! consumers is a stylesheet sharing a document with libraries that declare
//! their own `--bg` and `--fg`. The prefix lives in the name rather than in the
//! consumer that needs it, so all three read the same string and no consumer
//! re-derives the mapping.
//!
//! These values are the ratified SIGNAL contract. A change here is a design
//! decision, never drift.
//!
//! NOTE: the `Token { name: …, value: …, role: … }` lines below are parsed by
//! `tools/check-theme-sync.sh` — keep `name` and `value` adjacent, one token
//! per line.
//!
//! Two modules carry the rest of the vocabulary, both equally data-only:
//!
//! * [`marks`] — the symbol inventory the terminal surface draws from, and the
//!   admission rule that governs what may enter it.
//! * [`tier`] — the colour-capability ladder, and the hand-authored map from a
//!   token to what each tier can actually show. It resolves a token to a slot
//!   NUMBER and stops there; turning a slot into bytes belongs to a renderer.
//!
//! [`swatch`] renders both into one text frame per tier — the goldens that are
//! this repository's public mirror of the private tier tables.

pub mod marks;
pub mod probe;
pub mod swatch;
pub mod tier;

pub use tier::{AnsiSlot, ColorChoice, ColorTier, Paint, TerminalEnv, Tier16};

/// A flag value the parser refuses. Shared by every closed-value flag on this
/// surface, so they all refuse the same way and all name what they DO accept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlagValueError {
    pub flag: &'static str,
    pub given: String,
    pub allowed: &'static [&'static str],
}

impl FlagValueError {
    pub fn new(flag: &'static str, given: &str, allowed: &'static [&'static str]) -> Self {
        FlagValueError {
            flag,
            given: given.to_owned(),
            allowed,
        }
    }
}

impl std::fmt::Display for FlagValueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}: no such value {:?}; one of: {}",
            self.flag,
            self.given,
            self.allowed.join(", ")
        )
    }
}

impl std::error::Error for FlagValueError {}

/// One named color token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, specta::Type)]
pub struct Token {
    /// snake_case token name (CSS uses the kebab-case form).
    pub name: &'static str,
    /// `#RRGGBB` hex color.
    pub value: &'static str,
    /// What the token is FOR — the contract is the role, not the hex.
    pub role: &'static str,
    /// The nearest xterm-256 cube index, measured once and recorded here.
    /// Precomputed rather than quantized at render time, which is also why it
    /// exists for the three tokens that are never painted: the table that
    /// measured it measured all fifteen.
    pub index256: u8,
    /// What sixteen colours does with the token. HAND-AUTHORED — see
    /// [`tier::Tier16`] for why computing it fails.
    pub tier16: tier::Tier16,
}

/// The namespace every token name carries.
///
/// A token name is also a CSS custom-property name — `gws_hue_bright` is
/// `--gws-hue-bright` — and the stylesheet that consumes them imports Fumadocs
/// and Tailwind, both of which declare bare colour roles of their own. A bare
/// `--bg` in that document is not this palette's `--bg`; it is whichever
/// stylesheet loaded last.
pub const CSS_PREFIX: &str = "gws";

/// The SIGNAL palette.
///
/// Twelve ratified tokens plus the three minted by ADR-0028 for the console's
/// structural roles. `gws_focus` carries `gws_hue`'s value on purpose — it is
/// minted as a distinct ROLE, not a distinct hex, and stays pinned there until
/// a rendered console proves it must diverge. Names are unique; values are not
/// required to be, because the contract is the role.
#[rustfmt::skip]
pub const SIGNAL: &[Token] = &[
    Token { name: "gws_bg", value: "#070B10", role: "canvas background", index256: 232, tier16: Tier16::NotAColor },
    Token { name: "gws_surface", value: "#121A24", role: "raised surface", index256: 234, tier16: Tier16::NotAColor },
    Token { name: "gws_surface_2", value: "#1F2B3A", role: "second elevation", index256: 235, tier16: Tier16::NotAColor },
    Token { name: "gws_hue", value: "#6BDBFF", role: "accent", index256: 81, tier16: Tier16::Slot(AnsiSlot::BrightCyan) },
    Token { name: "gws_hue_dim", value: "#3FA8CC", role: "accent, dimmed", index256: 38, tier16: Tier16::Slot(AnsiSlot::Cyan) },
    Token { name: "gws_hue_bright", value: "#9AE8FF", role: "accent, bright", index256: 117, tier16: Tier16::BoldSlot(AnsiSlot::BrightCyan) },
    Token { name: "gws_fg", value: "#E4EDF5", role: "foreground text", index256: 255, tier16: Tier16::Reset },
    Token { name: "gws_faint", value: "#526274", role: "faint structure (decorative/hairline only — never text or essential UI)", index256: 59, tier16: Tier16::Dropped },
    Token { name: "gws_muted", value: "#9AA5AF", role: "muted text", index256: 248, tier16: Tier16::Slot(AnsiSlot::BrightBlack) },
    Token { name: "gws_warn", value: "#F2C14E", role: "warning", index256: 221, tier16: Tier16::Slot(AnsiSlot::BrightYellow) },
    Token { name: "gws_fail", value: "#FF6E6E", role: "failure", index256: 203, tier16: Tier16::Slot(AnsiSlot::BrightRed) },
    Token { name: "gws_ok", value: "#6EE7A8", role: "success", index256: 78, tier16: Tier16::Slot(AnsiSlot::BrightGreen) },
    Token { name: "gws_border", value: "#748496", role: "solid structural boundary", index256: 244, tier16: Tier16::Slot(AnsiSlot::White) },
    Token { name: "gws_focus", value: "#6BDBFF", role: "focused-pane / control indicator", index256: 81, tier16: Tier16::ReverseVideo },
    Token { name: "gws_selection", value: "#F5F9FD", role: "selected-row foreground", index256: 231, tier16: Tier16::ReverseVideo },
];

/// One token's light-polarity value.
///
/// Deliberately NOT a [`Token`]. `index256` and `tier16` answer "what does a
/// terminal do with this", and a terminal's answer does not change because a
/// browser is in light mode — the TUI paints [`SIGNAL`] and only [`SIGNAL`].
/// Giving the light palette those fields would invite someone to render from
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LightToken {
    /// The [`Token::name`] this is the light value FOR.
    pub name: &'static str,
    /// `#RRGGBB` hex color.
    pub value: &'static str,
}

/// SIGNAL at light polarity — the same fifteen roles, same order, inverted ground.
///
/// **`gws_hue_bright` is the DARKEST cyan here and `gws_hue_dim` the lightest,
/// which looks backwards and is not.** The roles are `accent, bright` and
/// `accent, dimmed`, and [`tier::Tier16`] resolves them to a bold slot and a
/// plain slot respectively: the axis is EMPHASIS, not luminance. On a near-black
/// ground more emphasis means lighter; on a near-white ground it means darker.
/// Carrying the luminance across instead of the meaning would have made the
/// high-emphasis accent the least readable thing on the page.
///
/// Every value here was chosen against a measured floor rather than by eye —
/// `light_palette_clears_its_contrast_floors` and the `assert_contrast_floors`
/// helper it shares with the dark palette are the record of which floor and
/// why. `gws_faint` is the one token exempt from the text floor, because its
/// ratified role forbids it from ever being text.
#[rustfmt::skip]
pub const SIGNAL_LIGHT: &[LightToken] = &[
    LightToken { name: "gws_bg", value: "#F7F9FC" },
    LightToken { name: "gws_surface", value: "#ECF1F6" },
    LightToken { name: "gws_surface_2", value: "#DEE6EE" },
    LightToken { name: "gws_hue", value: "#08657D" },
    LightToken { name: "gws_hue_dim", value: "#2C6B7E" },
    LightToken { name: "gws_hue_bright", value: "#054C60" },
    LightToken { name: "gws_fg", value: "#0A1018" },
    LightToken { name: "gws_faint", value: "#97A5B4" },
    LightToken { name: "gws_muted", value: "#45525F" },
    LightToken { name: "gws_warn", value: "#7F5300" },
    LightToken { name: "gws_fail", value: "#A0201B" },
    LightToken { name: "gws_ok", value: "#0A6338" },
    LightToken { name: "gws_border", value: "#6E7E8F" },
    LightToken { name: "gws_focus", value: "#08657D" },
    LightToken { name: "gws_selection", value: "#04090E" },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// sRGB relative luminance, WCAG 2.1 §relative-luminance.
    fn luminance(hex: &str) -> f64 {
        let channel = |offset: usize| {
            let byte = u8::from_str_radix(&hex[offset..offset + 2], 16).expect("hex pair");
            let c = f64::from(byte) / 255.0;
            if c <= 0.040_45 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(1) + 0.7152 * channel(3) + 0.0722 * channel(5)
    }

    /// WCAG contrast ratio, 1.0 (identical) to 21.0 (black on white).
    fn contrast(a: &str, b: &str) -> f64 {
        let (x, y) = (luminance(a), luminance(b));
        let (hi, lo) = if x > y { (x, y) } else { (y, x) };
        (hi + 0.05) / (lo + 0.05)
    }

    #[test]
    fn fifteen_unique_snake_named_hex_tokens() {
        assert_eq!(SIGNAL.len(), 15);
        let mut names: Vec<&str> = SIGNAL.iter().map(|t| t.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 15, "duplicate token name");
        for token in SIGNAL {
            assert!(
                token
                    .name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "token name not snake_case: {}",
                token.name
            );
            assert!(
                token.value.len() == 7
                    && token.value.starts_with('#')
                    && token.value[1..].chars().all(|c| c.is_ascii_hexdigit()),
                "token value not #RRGGBB: {}",
                token.value
            );
            assert!(!token.role.is_empty());
        }
    }

    #[test]
    fn every_name_carries_the_css_namespace() {
        // The reason this is a test and not a convention: the collision it
        // prevents is invisible from Rust. A token added as `link` compiles,
        // renders correctly in every TUI tier, passes the swatch goldens, and
        // then resolves in the browser to whatever Fumadocs or Tailwind last
        // wrote to `--link`. The stylesheet is the only consumer that can be
        // wrong about a name, and it is the one consumer with no compiler.
        for token in SIGNAL {
            let bare = token
                .name
                .strip_prefix(CSS_PREFIX)
                // `gws_hue` yes, `gwsomething` no — the separator is the check.
                .and_then(|rest| rest.strip_prefix('_'));
            assert!(
                bare.is_some_and(|name| !name.is_empty()),
                "token {:?} is not namespaced: every name must be `{}_<role>`, \
                 because the name IS the CSS custom property",
                token.name,
                CSS_PREFIX
            );
        }
    }

    #[test]
    fn the_light_palette_covers_signal_exactly_and_in_order() {
        // Order, not just membership. Both tables are read positionally by
        // anyone diffing them, and a light palette that silently omitted a
        // token would fail as a missing CSS property three layers away, in
        // `check-theme-sync.sh`, naming the stylesheet rather than this file.
        let dark: Vec<&str> = SIGNAL.iter().map(|t| t.name).collect();
        let light: Vec<&str> = SIGNAL_LIGHT.iter().map(|t| t.name).collect();
        assert_eq!(light, dark, "the light palette must mirror SIGNAL");

        for token in SIGNAL_LIGHT {
            assert!(
                token.value.len() == 7
                    && token.value.starts_with('#')
                    && token.value[1..].chars().all(|c| c.is_ascii_hexdigit()),
                "light value not #RRGGBB: {}",
                token.value
            );
        }
    }

    #[test]
    fn light_focus_shares_light_hues_value_too() {
        // The ratification is about the ROLE sharing a hex, not about one
        // specific hex. A light palette that split them would have quietly
        // un-ratified ADR-0028 residual 1 for half the product.
        let value = |name: &str| {
            SIGNAL_LIGHT
                .iter()
                .find(|token| token.name == name)
                .map(|token| token.value)
                .expect("token")
        };
        assert_eq!(value("gws_focus"), value("gws_hue"));
    }

    /// The contrast floors, in the one place both polarities are held to them.
    ///
    /// Measured against the DEEPEST ground a token can sit on — `surface_2`,
    /// not `bg`. Checking against the page background is the comfortable
    /// mistake: it passes for a colour that is unreadable in every card and
    /// panel on the page, which is where most of this text actually lives.
    ///
    /// 4.5:1 is WCAG AA for body text. `gws_border` gets 3.0:1, the non-text
    /// UI-component floor, because it draws boundaries rather than words.
    /// `gws_faint` is absent on purpose: its ratified role forbids text and
    /// essential UI outright, so a text floor would be asserting something the
    /// role already rules out.
    fn assert_contrast_floors(palette: &[(&str, &str)], polarity: &str) {
        // Count before verdict. Every assertion below is a fold, and a fold
        // cannot tell "all eight cleared" from "the palette arrived empty" —
        // a caller that normalized through a mistyped filter would collect
        // nothing and be congratulated for it.
        assert_eq!(
            palette.len(),
            15,
            "{polarity}: expected the full 15-token palette, got {}",
            palette.len()
        );

        let value = |name: &str| {
            palette
                .iter()
                .find(|(token, _)| *token == name)
                .map(|(_, value)| *value)
                .unwrap_or_else(|| panic!("{polarity} palette has no {name}"))
        };
        let deepest = value("gws_surface_2");

        for name in [
            "gws_fg",
            "gws_muted",
            "gws_hue",
            "gws_hue_dim",
            "gws_hue_bright",
            "gws_warn",
            "gws_fail",
            "gws_ok",
        ] {
            let ratio = contrast(value(name), deepest);
            assert!(
                ratio >= 4.5,
                "{polarity}: {name} is {ratio:.2}:1 on gws_surface_2, below the 4.5:1 text floor"
            );
        }

        let border = contrast(value("gws_border"), deepest);
        assert!(
            border >= 3.0,
            "{polarity}: gws_border is {border:.2}:1 on gws_surface_2, below the 3.0:1 UI floor"
        );
    }

    #[test]
    fn light_palette_clears_its_contrast_floors() {
        let palette: Vec<(&str, &str)> = SIGNAL_LIGHT.iter().map(|t| (t.name, t.value)).collect();
        assert_contrast_floors(&palette, "light");
    }

    #[test]
    fn dark_palette_clears_its_contrast_floors() {
        // The polarity most readers actually see — `globals.css` sets
        // `color-scheme: dark` on `:root` and the crate's default is this
        // palette — and until now the only one with no asserted floor. Light
        // got a test because its values were being chosen against a measurement
        // at the time; dark predated that and was simply never revisited.
        //
        // It passes, and passed before this test existed: the worst text token
        // is `gws_hue_dim` at 5.25:1 against a 4.5 floor, and `gws_border` sits
        // at 3.74:1 against 3.0 — more headroom than light has, whose worst is
        // 4.74:1. That is the argument for pinning it rather than against.
        // Nothing held it there, so a future palette edit could have dropped
        // the default polarity below the floor with every gate still green,
        // and the asymmetry itself was the tell: one polarity measured, one
        // trusted, no reason recorded for the difference.
        let palette: Vec<(&str, &str)> = SIGNAL.iter().map(|t| (t.name, t.value)).collect();
        assert_contrast_floors(&palette, "dark");
    }

    #[test]
    fn emphasis_runs_the_same_direction_in_both_polarities() {
        // The property the light palette is easiest to get wrong: `hue_bright`
        // is the DARKEST cyan in light, which reads as a typo until you know
        // the axis is emphasis rather than luminance. Pinned so that "fixing"
        // it fails here instead of shipping an accent that disappears.
        let against = |palette: &[(&str, &str)], ground: &str| {
            palette
                .iter()
                .map(|(_, value)| contrast(value, ground))
                .collect::<Vec<f64>>()
        };

        let dark_values: Vec<(&str, &str)> = SIGNAL
            .iter()
            .filter(|t| t.name.starts_with("gws_hue"))
            .map(|t| (t.name, t.value))
            .collect();
        let light_values: Vec<(&str, &str)> = SIGNAL_LIGHT
            .iter()
            .filter(|t| t.name.starts_with("gws_hue"))
            .map(|t| (t.name, t.value))
            .collect();

        let dark_bg = SIGNAL[0].value;
        let light_bg = SIGNAL_LIGHT[0].value;
        // Declaration order is hue, hue_dim, hue_bright.
        for (label, ratios) in [
            ("dark", against(&dark_values, dark_bg)),
            ("light", against(&light_values, light_bg)),
        ] {
            let (hue, dim, bright) = (ratios[0], ratios[1], ratios[2]);
            assert!(
                dim < hue && hue < bright,
                "{label}: emphasis must climb dim({dim:.2}) < hue({hue:.2}) < bright({bright:.2})"
            );
        }
    }

    #[test]
    fn faint_role_forbids_text_and_essential_ui() {
        let faint = SIGNAL
            .iter()
            .find(|token| token.name == "gws_faint")
            .expect("faint token");
        assert_eq!(
            faint.role,
            "faint structure (decorative/hairline only — never text or essential UI)"
        );
    }

    #[test]
    fn focus_shares_hues_value_on_purpose() {
        // ADR-0028 residual 1, made mechanical: `focus` is a distinct role on a
        // shared hex, and the shared hex is the decision rather than an
        // oversight. Anyone "fixing" the duplicate has to delete this test and
        // say why — every candidate fourth cyan measured 7-9 dE from an
        // existing member of the three-intensity family, which is accent
        // collapse, not a family member.
        let value = |name: &str| {
            SIGNAL
                .iter()
                .find(|token| token.name == name)
                .map(|token| token.value)
                .expect("token")
        };
        assert_eq!(value("gws_focus"), value("gws_hue"));
    }

    #[test]
    fn token_serializes_as_plain_data() {
        let json = serde_json::to_value(SIGNAL[0]).expect("serialize");
        assert_eq!(
            json,
            serde_json::json!({
                "name": "gws_bg",
                "value": "#070B10",
                "role": "canvas background",
                "index256": 232,
                "tier16": "NotAColor",
            })
        );
    }
}
