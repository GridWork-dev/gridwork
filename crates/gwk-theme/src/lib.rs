//! The SIGNAL design tokens — data only, no rendering, no dependencies beyond
//! serialization.
//!
//! One source of truth consumed three ways: the site's CSS custom properties
//! (checked by `tools/check-theme-sync.sh` — token `name` maps to
//! `--kebab-case`), the TUI theme, and the generated TypeScript contract.
//!
//! These values are pre-ratification carriers: the design taste-gate ratifies
//! or adjusts them, and a change here is a design decision, never drift.
//!
//! NOTE: the `Token { name: …, value: …, role: … }` lines below are parsed by
//! `tools/check-theme-sync.sh` — keep one token per line.

/// One named color token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, specta::Type)]
pub struct Token {
    /// snake_case token name (CSS uses the kebab-case form).
    pub name: &'static str,
    /// `#RRGGBB` hex color.
    pub value: &'static str,
    /// What the token is FOR — the contract is the role, not the hex.
    pub role: &'static str,
}

/// The SIGNAL palette.
#[rustfmt::skip]
pub const SIGNAL: &[Token] = &[
    Token { name: "bg", value: "#070B10", role: "canvas background" },
    Token { name: "surface", value: "#0D141D", role: "raised surface" },
    Token { name: "surface_2", value: "#121B26", role: "second elevation" },
    Token { name: "hue", value: "#6BDBFF", role: "accent" },
    Token { name: "hue_dim", value: "#3FA8CC", role: "accent, dimmed" },
    Token { name: "hue_bright", value: "#9AE8FF", role: "accent, bright" },
    Token { name: "fg", value: "#CBD7E2", role: "foreground text" },
    Token { name: "faint", value: "#526274", role: "faint structure" },
    Token { name: "muted", value: "#909AA3", role: "muted text" },
    Token { name: "warn", value: "#F2C14E", role: "warning" },
    Token { name: "fail", value: "#FF6E6E", role: "failure" },
    Token { name: "ok", value: "#6EE7A8", role: "success" },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn twelve_unique_snake_named_hex_tokens() {
        assert_eq!(SIGNAL.len(), 12);
        let mut names: Vec<&str> = SIGNAL.iter().map(|t| t.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 12, "duplicate token name");
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
    fn token_serializes_as_plain_data() {
        let json = serde_json::to_value(SIGNAL[0]).expect("serialize");
        assert_eq!(
            json,
            serde_json::json!({ "name": "bg", "value": "#070B10", "role": "canvas background" })
        );
    }
}
