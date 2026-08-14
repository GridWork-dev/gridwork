//! Colour-capability tiers, and the hand-authored map from a token to what a
//! terminal can actually show at each one.
//!
//! # Three rules this module exists to hold
//!
//! **Resolve capability once, at startup.** One [`ColorTier`] for the process.
//! No `if truecolor` scattered through widget code — every layer of the
//! detection stack can lie, and a program that asks the question twice can
//! answer it twice.
//!
//! **Never set a background.** A theme that paints its own background fights
//! three separate user settings at once: their colour scheme, their
//! transparency, and their light/dark mode. So the three elevation tokens have
//! no foreground expression at any tier and [`Token::paint`] says so; their
//! measured indices are recorded below because the table that measured them
//! recorded them, not because anything hands them back.
//!
//! **Never quantize at render time.** The 256 index and the 16 slot are
//! constants next to the hexes. Tier 16 in particular is HAND-AUTHORED and
//! never computed: naive nearest-colour quantization collapses all three
//! elevation steps onto slot 0, lands `hue` and `ok` on the same slot, and
//! turns `fail` into a dark maroon at ΔE 41 — muddy, dark, and wrong for a
//! status signal. The sixteen slots are user-owned and user-themed; you cannot
//! compute against them because you do not know what they are. At this tier the
//! palette stops being a palette and becomes a semantics contract, which is
//! what `Token::role` already is.

use crate::{FlagValueError, Token};

/// What the terminal can show. Resolved once; see [`ColorTier::resolve`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ColorTier {
    Truecolor,
    Xterm256,
    Ansi16,
    Mono,
}

impl ColorTier {
    /// Every tier, brightest first — the order the swatch renders in.
    pub const ALL: &'static [ColorTier] = &[
        ColorTier::Truecolor,
        ColorTier::Xterm256,
        ColorTier::Ansi16,
        ColorTier::Mono,
    ];

    /// The name used in flag values, goldens and diagnostics.
    pub const fn as_str(self) -> &'static str {
        match self {
            ColorTier::Truecolor => "truecolor",
            ColorTier::Xterm256 => "xterm256",
            ColorTier::Ansi16 => "ansi16",
            ColorTier::Mono => "mono",
        }
    }

    /// The published detection stack, in order, with the explicit choice first.
    ///
    /// This is not an improvement on the stack everyone else uses — it is that
    /// stack, written down. Assume `TERM=xterm-256color` even on a truecolor
    /// terminal, and assume a multiplexer quantizes 24-bit colour to the cube
    /// unless the outer terminfo advertises otherwise. The consequence is
    /// load-bearing: **no distinction this surface relies on may be finer than
    /// the 6×6×6 cube.**
    pub fn resolve(env: &TerminalEnv) -> ColorTier {
        Self::resolve_loud(env).0
    }

    /// [`Self::resolve`], plus whether the answer was the unrecognized-TERM
    /// fallthrough: Mono chosen not because anyone asked for it — no flag, no
    /// `NO_COLOR`, no dumb terminal, no pipe — but because the terminal named
    /// itself something the ladder does not know. That is the stack's one
    /// silent degradation, and a caller that talks to an operator says it out
    /// loud instead of letting an exotic TERM look like a broken palette.
    pub fn resolve_loud(env: &TerminalEnv) -> (ColorTier, bool) {
        // 1. The explicit choice wins, always.
        match env.color {
            ColorChoice::Never => return (ColorTier::Mono, false),
            ColorChoice::Ansi16 => return (ColorTier::Ansi16, false),
            ColorChoice::Xterm256 => return (ColorTier::Xterm256, false),
            ColorChoice::Truecolor => return (ColorTier::Truecolor, false),
            ColorChoice::Auto => {}
        }
        // 2. NO_COLOR is absolute. Rendering a palette under it is a bug, not a
        //    branding decision — which is why it sits above the force variables
        //    rather than racing them.
        if env.no_color.as_deref().is_some_and(|v| !v.is_empty()) {
            return (ColorTier::Mono, false);
        }
        let term = env.term.as_deref().unwrap_or_default();
        // 3. A terminal that says it is dumb is believed.
        if term == "dumb" {
            return (ColorTier::Mono, false);
        }
        // 4. The force variables, and this is the one placement the published
        //    stack leaves open: they sit HERE, above the tty check, because
        //    forcing colour into a pipe is the entire reason they exist. Below
        //    it they would be unreachable in the only case anyone sets them.
        //    CLICOLOR_FORCE is read first for no better reason than that it is
        //    the older of the two; they never disagree in practice.
        match force(env.clicolor_force.as_deref()).or_else(|| force(env.force_color.as_deref())) {
            Some(Force::Off) => return (ColorTier::Mono, false),
            Some(Force::Depth(tier)) => return (tier, false),
            // Forced on without naming a depth: skip the tty check, keep detecting.
            Some(Force::Unspecified) => {}
            // 5. Not a terminal, and nobody forced it.
            None if !env.stdout_is_tty => return (ColorTier::Mono, false),
            None => {}
        }
        let colorterm = env.colorterm.as_deref().unwrap_or_default();
        // 6-9. Detection proper. The first two published steps — COLORTERM
        //      saying truecolor or 24bit, and a `-direct` terminfo entry — are
        //      one branch here because they reach the same tier; they stay two
        //      conditions because they are two independent claims and either
        //      alone is sufficient.
        if colorterm == "truecolor" || colorterm == "24bit" || term.ends_with("-direct") {
            (ColorTier::Truecolor, false)
        } else if term.ends_with("-256color") {
            (ColorTier::Xterm256, false)
        } else if ["xterm", "screen", "tmux", "rxvt"]
            .iter()
            .any(|family| term.contains(family))
        {
            (ColorTier::Ansi16, false)
        } else {
            (ColorTier::Mono, true)
        }
    }
}

/// The `--color=` value. Exactly five spellings, no others.
///
/// The flag is the documented escape for a terminal whose capability the stack
/// above gets wrong, and for the one case detection can never cover: a user
/// whose sixteen slots are themed the way they want them and who would rather
/// have those than a palette. `--color=16` renders into the user's own slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorChoice {
    #[default]
    Auto,
    Never,
    Ansi16,
    Xterm256,
    Truecolor,
}

impl ColorChoice {
    pub const FLAG: &'static str = "--color";
    pub const VALUES: &'static [&'static str] = &["auto", "never", "16", "256", "truecolor"];

    /// Parse a `--color=` value. An unknown one is refused rather than
    /// absorbed: silently falling back to `auto` would let `--color=ansi16`
    /// look like it worked.
    pub fn parse(value: &str) -> Result<Self, FlagValueError> {
        match value {
            "auto" => Ok(ColorChoice::Auto),
            "never" => Ok(ColorChoice::Never),
            "16" => Ok(ColorChoice::Ansi16),
            "256" => Ok(ColorChoice::Xterm256),
            "truecolor" => Ok(ColorChoice::Truecolor),
            other => Err(FlagValueError::new(Self::FLAG, other, Self::VALUES)),
        }
    }
}

/// Everything [`ColorTier::resolve`] reads, gathered in one place.
///
/// A struct rather than direct environment access, so the resolution order is
/// testable without a subprocess and without mutating the process environment
/// from a test that runs in parallel with fifteen others.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalEnv {
    /// The parsed `--color=` value, or [`ColorChoice::Auto`].
    pub color: ColorChoice,
    pub no_color: Option<String>,
    pub clicolor_force: Option<String>,
    pub force_color: Option<String>,
    pub colorterm: Option<String>,
    pub term: Option<String>,
    pub stdout_is_tty: bool,
}

impl TerminalEnv {
    /// Read the real environment once. The caller supplies the parsed flag,
    /// because the flag belongs to whichever binary owns the command line.
    pub fn from_process(color: ColorChoice) -> Self {
        use std::io::IsTerminal;
        TerminalEnv {
            color,
            no_color: std::env::var("NO_COLOR").ok(),
            clicolor_force: std::env::var("CLICOLOR_FORCE").ok(),
            force_color: std::env::var("FORCE_COLOR").ok(),
            colorterm: std::env::var("COLORTERM").ok(),
            term: std::env::var("TERM").ok(),
            stdout_is_tty: std::io::stdout().is_terminal(),
        }
    }
}

enum Force {
    Off,
    Depth(ColorTier),
    /// Set to something that is not a depth. Forces colour on and lets the
    /// detection below pick how much.
    Unspecified,
}

fn force(value: Option<&str>) -> Option<Force> {
    // Set-but-empty is not set. `FORCE_COLOR=` in a shell profile is how a
    // variable gets UNset in practice, and reading it as "force on" would be
    // the opposite of what was meant.
    let value = value.filter(|v| !v.is_empty())?;
    Some(match value {
        "0" | "false" => Force::Off,
        "1" => Force::Depth(ColorTier::Ansi16),
        "2" => Force::Depth(ColorTier::Xterm256),
        "3" => Force::Depth(ColorTier::Truecolor),
        _ => Force::Unspecified,
    })
}

/// One of the sixteen slots the user's own theme owns.
///
/// ANSI names, not any particular library's: several rendering crates call slot
/// 7 "gray", slot 8 "dark gray" and slot 15 "white", which is three chances to
/// be exactly one slot off. Declaration order IS slot order, so `as u8` is the
/// index — pinned by `tier_slot_declaration_order_is_slot_order`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, specta::Type)]
pub enum AnsiSlot {
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

impl AnsiSlot {
    #[rustfmt::skip]
    pub const ALL: &'static [AnsiSlot] = &[
        AnsiSlot::Black, AnsiSlot::Red, AnsiSlot::Green, AnsiSlot::Yellow,
        AnsiSlot::Blue, AnsiSlot::Magenta, AnsiSlot::Cyan, AnsiSlot::White,
        AnsiSlot::BrightBlack, AnsiSlot::BrightRed, AnsiSlot::BrightGreen, AnsiSlot::BrightYellow,
        AnsiSlot::BrightBlue, AnsiSlot::BrightMagenta, AnsiSlot::BrightCyan, AnsiSlot::BrightWhite,
    ];

    /// 0-15.
    pub const fn index(self) -> u8 {
        self as u8
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            AnsiSlot::Black => "black",
            AnsiSlot::Red => "red",
            AnsiSlot::Green => "green",
            AnsiSlot::Yellow => "yellow",
            AnsiSlot::Blue => "blue",
            AnsiSlot::Magenta => "magenta",
            AnsiSlot::Cyan => "cyan",
            AnsiSlot::White => "white",
            AnsiSlot::BrightBlack => "bright_black",
            AnsiSlot::BrightRed => "bright_red",
            AnsiSlot::BrightGreen => "bright_green",
            AnsiSlot::BrightYellow => "bright_yellow",
            AnsiSlot::BrightBlue => "bright_blue",
            AnsiSlot::BrightMagenta => "bright_magenta",
            AnsiSlot::BrightCyan => "bright_cyan",
            AnsiSlot::BrightWhite => "bright_white",
        }
    }
}

/// What tier 16 does with a token. Six dispositions, and the last three are
/// three DIFFERENT things for three different reasons — collapsing any of them
/// into "no colour" loses the reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, specta::Type)]
pub enum Tier16 {
    /// Paint one of the user's own slots.
    Slot(AnsiSlot),
    /// Paint a slot and add bold. Bold is the escalation channel here:
    /// `hue_bright` shares slot 14 with `hue`, so weight is what separates
    /// them once the palette is gone.
    BoldSlot(AnsiSlot),
    /// The user's own foreground. `fg` has no slot of its own by design — the
    /// baseline text of a terminal program belongs to the terminal.
    Reset,
    /// **Dropped.** No expression at this tier at all. `faint` would collide
    /// with `muted` on slot 8, and it is decorative-only anyway, so it stops
    /// existing rather than becoming a second muted.
    Dropped,
    /// **No slot needed.** Reverse video and the column-zero accent cell carry
    /// the role outright at sixteen colours and below. A different disposition
    /// from `Dropped` for a different reason: the role SURVIVES here, colour
    /// just is not how it is carried. Reverse video is the one attribute that
    /// works at every tier including monochrome, and the only one that is
    /// background-agnostic.
    ReverseVideo,
    /// Not a foreground colour in a terminal at any tier. The three elevation
    /// steps become reverse video, box rules and blank-line grouping instead.
    NotAColor,
}

/// What a token resolves to. Produced only by [`Token::paint`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Paint {
    Rgb(u8, u8, u8),
    Indexed(u8),
    Slot(AnsiSlot),
    BoldSlot(AnsiSlot),
    /// The user's own foreground.
    Reset,
    /// Colour carries nothing here; the reverse-video attribute does.
    ReverseVideo,
    /// Paint nothing. Either the token is dropped at this tier or it was never
    /// a foreground colour to begin with.
    Unpainted,
}

impl Token {
    /// The token's value as three bytes.
    ///
    /// Total by construction: `fifteen_unique_snake_named_hex_tokens` proves
    /// every value is `#RRGGBB`, and `tier_token_hexes_round_trip` re-formats
    /// each parse back to its literal, so the unreachable branch below can
    /// never be reached silently.
    pub const fn rgb(&self) -> (u8, u8, u8) {
        let bytes = self.value.as_bytes();
        (hex_byte(bytes, 1), hex_byte(bytes, 3), hex_byte(bytes, 5))
    }

    /// Resolve the token at a tier. **The only place a token becomes a colour.**
    pub const fn paint(&self, tier: ColorTier) -> Paint {
        // The never-set-a-background rule, applied first and at every tier: the
        // elevation tokens are not foreground colours, and handing one back
        // would be handing a caller the user's background to overwrite.
        if matches!(self.tier16, Tier16::NotAColor) {
            return Paint::Unpainted;
        }
        match tier {
            ColorTier::Truecolor => {
                let (r, g, b) = self.rgb();
                Paint::Rgb(r, g, b)
            }
            ColorTier::Xterm256 => Paint::Indexed(self.index256),
            ColorTier::Ansi16 => match self.tier16 {
                Tier16::Slot(slot) => Paint::Slot(slot),
                Tier16::BoldSlot(slot) => Paint::BoldSlot(slot),
                Tier16::Reset => Paint::Reset,
                Tier16::ReverseVideo => Paint::ReverseVideo,
                Tier16::Dropped | Tier16::NotAColor => Paint::Unpainted,
            },
            // Mono is DERIVED from the tier-16 disposition rather than
            // hand-authored a second time, and the derivation is itself the
            // ratified claim: a washed tier 16 degrades to monochrome
            // SEMANTICS, not to nothing. A token that had a slot keeps its
            // meaning in the user's own foreground; a token carried by reverse
            // video is carried by reverse video at every tier, which is exactly
            // why reverse video was chosen as the floor; a token that was
            // dropped stays dropped, because a tier with fewer channels cannot
            // resurrect one.
            //
            // Bold does NOT survive the derivation. At sixteen colours it
            // exists to separate `hue_bright` from `hue`, which share slot 14.
            // At monochrome there are no slots to separate, so there is nothing
            // to escalate away from and no ratified row assigns weight here.
            ColorTier::Mono => match self.tier16 {
                Tier16::Slot(_) | Tier16::BoldSlot(_) | Tier16::Reset => Paint::Reset,
                Tier16::ReverseVideo => Paint::ReverseVideo,
                Tier16::Dropped | Tier16::NotAColor => Paint::Unpainted,
            },
        }
    }
}

const fn hex_byte(bytes: &[u8], at: usize) -> u8 {
    if at + 1 >= bytes.len() {
        return 0;
    }
    nibble(bytes[at]) * 16 + nibble(bytes[at + 1])
}

const fn nibble(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0,
    }
}

impl std::fmt::Display for Tier16 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Tier16::Slot(slot) => write!(f, "slot {} {}", slot.index(), slot.as_str()),
            Tier16::BoldSlot(slot) => {
                write!(f, "slot {} {} + bold", slot.index(), slot.as_str())
            }
            Tier16::Reset => f.write_str("reset"),
            Tier16::Dropped => f.write_str("dropped"),
            Tier16::ReverseVideo => f.write_str("reverse video"),
            Tier16::NotAColor => f.write_str("not a colour"),
        }
    }
}

impl std::fmt::Display for Paint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Paint::Rgb(r, g, b) => write!(f, "rgb #{r:02X}{g:02X}{b:02X}"),
            Paint::Indexed(index) => write!(f, "index {index}"),
            Paint::Slot(slot) => write!(f, "slot {} {}", slot.index(), slot.as_str()),
            Paint::BoldSlot(slot) => write!(f, "slot {} {} + bold", slot.index(), slot.as_str()),
            Paint::Reset => f.write_str("reset"),
            Paint::ReverseVideo => f.write_str("reverse video"),
            Paint::Unpainted => f.write_str("unpainted"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::SIGNAL;

    fn token(name: &str) -> &'static Token {
        SIGNAL
            .iter()
            .find(|token| token.name == name)
            .unwrap_or_else(|| panic!("no token {name:?}"))
    }

    #[test]
    fn tier_token_hexes_round_trip() {
        for entry in SIGNAL {
            let (r, g, b) = entry.rgb();
            assert_eq!(
                format!("#{r:02X}{g:02X}{b:02X}"),
                entry.value.to_uppercase(),
                "token {:?} does not round-trip through rgb()",
                entry.name
            );
        }
    }

    #[test]
    fn tier_slot_declaration_order_is_slot_order() {
        for (expected, slot) in AnsiSlot::ALL.iter().enumerate() {
            assert_eq!(slot.index() as usize, expected, "{slot:?}");
        }
        assert_eq!(AnsiSlot::ALL.len(), 16);
    }

    #[test]
    fn tier_256_indices_are_unique_except_the_by_construction_share() {
        // The index audit. One collision exists and it is deliberate: `focus`
        // shares `hue`'s index because it shares `hue`'s value. Every other
        // index across all fifteen tokens is its own.
        let mut seen: Vec<(u8, &str)> = SIGNAL.iter().map(|t| (t.index256, t.name)).collect();
        seen.sort_unstable();
        let collisions: Vec<_> = seen
            .windows(2)
            .filter(|pair| pair[0].0 == pair[1].0)
            .map(|pair| (pair[0].1, pair[1].1))
            .collect();
        assert_eq!(collisions, vec![("gws_focus", "gws_hue")]);
        assert_eq!(token("gws_focus").value, token("gws_hue").value);
    }

    #[test]
    fn tier_16_is_hand_authored_and_not_naive_quantization() {
        // The named failures of computing this table instead of authoring it.
        // Every one of these is what nearest-colour quantization actually
        // produces, and every one of them is wrong.
        assert_eq!(
            token("gws_fail").tier16,
            Tier16::Slot(AnsiSlot::BrightRed),
            "quantization puts fail on slot 1, a dark maroon at dE 41"
        );
        assert_eq!(
            token("gws_warn").tier16,
            Tier16::Slot(AnsiSlot::BrightYellow),
            "quantization puts warn on slot 3, an olive at dE 34.7"
        );
        // The elevation ramp does not collapse onto slot 0, because it does not
        // reach the slots at all.
        for name in ["gws_bg", "gws_surface", "gws_surface_2"] {
            assert_eq!(token(name).tier16, Tier16::NotAColor, "{name}");
        }
        // hue and ok both quantize onto slot 14; the authored map keeps them
        // apart, and hue_bright separates from hue by weight rather than by
        // taking a slot that means something else.
        assert_eq!(token("gws_hue").tier16, Tier16::Slot(AnsiSlot::BrightCyan));
        assert_eq!(token("gws_ok").tier16, Tier16::Slot(AnsiSlot::BrightGreen));
        assert_eq!(
            token("gws_hue_bright").tier16,
            Tier16::BoldSlot(AnsiSlot::BrightCyan)
        );
    }

    #[test]
    fn tier_dropped_and_no_slot_are_different_dispositions() {
        // `faint` is DROPPED — it has no expression at sixteen colours and
        // would collide with `muted` if it tried.
        assert_eq!(token("gws_faint").tier16, Tier16::Dropped);
        assert_eq!(
            token("gws_faint").paint(ColorTier::Ansi16),
            Paint::Unpainted
        );
        // `focus` and `selection` have NO SLOT — reverse video and the accent
        // cell carry them. The role survives; only the colour does not.
        for name in ["gws_focus", "gws_selection"] {
            assert_eq!(token(name).tier16, Tier16::ReverseVideo, "{name}");
            assert_eq!(
                token(name).paint(ColorTier::Ansi16),
                Paint::ReverseVideo,
                "{name}"
            );
        }
        // `border` is the one minted role that needed a slot at all: a box rule
        // cannot be drawn by reverse video, and `faint` is not available.
        assert_eq!(token("gws_border").tier16, Tier16::Slot(AnsiSlot::White));
    }

    #[test]
    fn tier_mono_keeps_reverse_video_and_loses_every_colour() {
        for entry in SIGNAL {
            match entry.paint(ColorTier::Mono) {
                Paint::Reset | Paint::ReverseVideo | Paint::Unpainted => {}
                other => panic!("token {:?} paints {other:?} at mono", entry.name),
            }
        }
        assert_eq!(
            token("gws_focus").paint(ColorTier::Mono),
            Paint::ReverseVideo,
            "reverse video is the tier-independent primitive, monochrome included"
        );
        assert_eq!(token("gws_fail").paint(ColorTier::Mono), Paint::Reset);
        assert_eq!(token("gws_faint").paint(ColorTier::Mono), Paint::Unpainted);
    }

    #[test]
    fn tier_the_elevation_tokens_are_never_painted_at_any_tier() {
        for tier in ColorTier::ALL {
            for name in ["gws_bg", "gws_surface", "gws_surface_2"] {
                assert_eq!(
                    token(name).paint(*tier),
                    Paint::Unpainted,
                    "{name} at {}",
                    tier.as_str()
                );
            }
        }
    }

    #[test]
    fn tier_every_other_token_paints_something_at_truecolor_and_256() {
        for entry in SIGNAL
            .iter()
            .filter(|t| t.tier16 != Tier16::NotAColor)
            .filter(|t| t.tier16 != Tier16::ReverseVideo)
        {
            assert!(matches!(entry.paint(ColorTier::Truecolor), Paint::Rgb(..)));
            assert!(matches!(
                entry.paint(ColorTier::Xterm256),
                Paint::Indexed(_)
            ));
        }
        // Except the two carried by reverse video, which are carried by reverse
        // video at the top of the ladder too — the tokens LAYER over the
        // primitive rather than replacing it. At truecolor and 256 they still
        // have a colour to layer with.
        for name in ["gws_focus", "gws_selection"] {
            assert!(matches!(
                token(name).paint(ColorTier::Truecolor),
                Paint::Rgb(..)
            ));
        }
    }

    // ── the detection stack ────────────────────────────────────────────────

    fn env(term: &str, colorterm: &str, tty: bool) -> TerminalEnv {
        TerminalEnv {
            term: Some(term.to_owned()),
            colorterm: (!colorterm.is_empty()).then(|| colorterm.to_owned()),
            stdout_is_tty: tty,
            ..TerminalEnv::default()
        }
    }

    #[test]
    fn tier_the_flag_beats_every_other_layer() {
        // Including the layers that are otherwise absolute. A user who asks for
        // sixteen colours on a truecolor terminal gets sixteen.
        let mut e = env("xterm-256color", "truecolor", true);
        e.no_color = Some("1".to_owned());
        e.color = ColorChoice::Ansi16;
        assert_eq!(ColorTier::resolve(&e), ColorTier::Ansi16);
        e.color = ColorChoice::Truecolor;
        assert_eq!(ColorTier::resolve(&e), ColorTier::Truecolor);
        e.color = ColorChoice::Never;
        assert_eq!(ColorTier::resolve(&e), ColorTier::Mono);
    }

    #[test]
    fn tier_no_color_and_dumb_and_no_tty_all_reach_mono() {
        let mut e = env("xterm-256color", "", true);
        e.no_color = Some("1".to_owned());
        assert_eq!(ColorTier::resolve(&e), ColorTier::Mono);
        // Set-but-empty is not set — that is how a variable gets unset.
        e.no_color = Some(String::new());
        assert_eq!(ColorTier::resolve(&e), ColorTier::Xterm256);

        assert_eq!(
            ColorTier::resolve(&env("dumb", "truecolor", true)),
            ColorTier::Mono
        );
        assert_eq!(
            ColorTier::resolve(&env("xterm-256color", "truecolor", false)),
            ColorTier::Mono
        );
    }

    #[test]
    fn tier_detection_walks_the_published_stack() {
        assert_eq!(
            ColorTier::resolve(&env("xterm-256color", "truecolor", true)),
            ColorTier::Truecolor
        );
        assert_eq!(
            ColorTier::resolve(&env("xterm-256color", "24bit", true)),
            ColorTier::Truecolor
        );
        assert_eq!(
            ColorTier::resolve(&env("xterm-direct", "", true)),
            ColorTier::Truecolor
        );
        assert_eq!(
            ColorTier::resolve(&env("xterm-256color", "", true)),
            ColorTier::Xterm256
        );
        assert_eq!(
            ColorTier::resolve(&env("tmux-256color", "", true)),
            ColorTier::Xterm256
        );
        assert_eq!(
            ColorTier::resolve(&env("xterm", "", true)),
            ColorTier::Ansi16
        );
        assert_eq!(
            ColorTier::resolve(&env("screen", "", true)),
            ColorTier::Ansi16
        );
        assert_eq!(
            ColorTier::resolve(&env("vt100", "", true)),
            ColorTier::Mono,
            "an unrecognised terminal gets monochrome, not a guess"
        );
    }

    #[test]
    fn tier_only_the_unrecognized_term_fallthrough_is_loud() {
        // The one silent degradation in the stack announces itself…
        assert_eq!(
            ColorTier::resolve_loud(&env("vt100", "", true)),
            (ColorTier::Mono, true)
        );
        // …while every deliberate road to Mono stays quiet: an answer the
        // operator asked for is not a degradation.
        let mut chosen = env("vt100", "", true);
        chosen.color = ColorChoice::Never;
        assert_eq!(
            ColorTier::resolve_loud(&chosen),
            (ColorTier::Mono, false),
            "--color=never"
        );
        let mut muted = env("vt100", "", true);
        muted.no_color = Some("1".to_owned());
        assert_eq!(
            ColorTier::resolve_loud(&muted),
            (ColorTier::Mono, false),
            "NO_COLOR"
        );
        assert_eq!(
            ColorTier::resolve_loud(&env("dumb", "", true)),
            (ColorTier::Mono, false),
            "TERM=dumb"
        );
        assert_eq!(
            ColorTier::resolve_loud(&env("vt100", "", false)),
            (ColorTier::Mono, false),
            "a pipe is not a degraded terminal"
        );
        assert_eq!(
            ColorTier::resolve_loud(&env("xterm-256color", "", true)),
            (ColorTier::Xterm256, false),
            "a recognized terminal is never loud"
        );
    }

    #[test]
    fn tier_the_force_variables_carry_a_depth_and_reach_a_pipe() {
        // The whole point of them: stdout is NOT a tty in any of these.
        for (value, expected) in [
            ("0", ColorTier::Mono),
            ("1", ColorTier::Ansi16),
            ("2", ColorTier::Xterm256),
            ("3", ColorTier::Truecolor),
        ] {
            let mut e = env("xterm-256color", "", false);
            e.force_color = Some(value.to_owned());
            assert_eq!(ColorTier::resolve(&e), expected, "FORCE_COLOR={value}");
            let mut e = env("xterm-256color", "", false);
            e.clicolor_force = Some(value.to_owned());
            assert_eq!(ColorTier::resolve(&e), expected, "CLICOLOR_FORCE={value}");
        }
        // Forced on without a depth: detection picks it, and the tty check is
        // skipped rather than failed.
        let mut e = env("xterm-256color", "", false);
        e.clicolor_force = Some("yes".to_owned());
        assert_eq!(ColorTier::resolve(&e), ColorTier::Xterm256);
        // NO_COLOR still wins over both.
        e.no_color = Some("1".to_owned());
        assert_eq!(ColorTier::resolve(&e), ColorTier::Mono);
    }

    #[test]
    fn tier_an_unknown_color_value_is_refused_rather_than_absorbed() {
        assert_eq!(ColorChoice::parse("16"), Ok(ColorChoice::Ansi16));
        assert_eq!(ColorChoice::parse("truecolor"), Ok(ColorChoice::Truecolor));
        let err = ColorChoice::parse("ansi16").expect_err("ansi16 is not a value");
        let text = err.to_string();
        assert!(text.contains("--color"), "{text}");
        assert!(text.contains("ansi16"), "{text}");
        // The message names what IS allowed, or the caller has to go read the
        // source to find out.
        for allowed in ColorChoice::VALUES {
            assert!(text.contains(allowed), "{text} omits {allowed}");
        }
        // And every advertised value parses, so the message cannot advertise a
        // spelling the parser refuses.
        for allowed in ColorChoice::VALUES {
            assert!(ColorChoice::parse(allowed).is_ok(), "{allowed}");
        }
    }

    #[test]
    fn tier_names_are_distinct_and_stable() {
        let names: BTreeSet<&str> = ColorTier::ALL.iter().map(|t| t.as_str()).collect();
        assert_eq!(names.len(), 4);
        // The golden filenames are built from these.
        assert_eq!(ColorTier::Xterm256.as_str(), "xterm256");
    }
}
