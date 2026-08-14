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

#[cfg(test)]
mod tests {
    use super::*;

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
