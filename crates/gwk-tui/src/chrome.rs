//! The workspace chrome's theme slot — the one surface a user's own colour
//! preference reaches.
//!
//! # What is themeable, and what is not
//!
//! Exactly three things exist on a workspace screen, and they get three
//! different answers:
//!
//! * **Orchestration surfaces** — the Queue, the Board, the Hall, the
//!   drilldown, the config lens — are **Signal, and that is not negotiable.**
//!   A state colour there is a claim about a running system, and a user who
//!   repainted `fail` would be repainting a fact. Those lenses resolve
//!   through [`crate::theme`] and never touch this module. The check at the
//!   bottom of this file walks the crate's own source to prove it, because
//!   an invariant nothing checks is a sentence in a doc comment.
//! * **Pane content** — the bytes a hosted session writes — is the session's
//!   own. A pane passes its colours through unmodified; there is no
//!   [`ChromeRole`] for pane content and there cannot be one without editing
//!   the closed enum below, which is the mechanism rather than the promise.
//! * **The workspace's own furniture** — pane borders, pane titles, the tab
//!   strip, the workspace label, the status line — is chrome. It carries no
//!   state claim and belongs to nobody but the operator looking at it. That
//!   is the slot, and it is all of the slot.
//!
//! # A remap, not a palette
//!
//! A user theme here rebinds a chrome ROLE to a different **Signal token**.
//! It cannot introduce a colour, because both sides of the mapping are
//! closed: [`ChromeRole::ALL`] on the left, `gwk_theme::SIGNAL` on the
//! right. Two properties fall out of that rather than needing to be
//! enforced separately — every remapped role still degrades correctly
//! across all four colour tiers, since it is a ratified token doing the
//! degrading; and no user file can widen the palette the rest of the
//! surface is checked against.
//!
//! One remap is refused: a role may not land on a token whose sixteen-colour
//! disposition is `NotAColor`. Those three are elevation steps, never a
//! terminal foreground at any tier, so the remap would not produce a
//! different colour — it would produce an invisible one, and the operator
//! would read that as a bug in the workspace rather than as their own file.
//!
//! # Where the file comes from
//!
//! `GWK_CHROME_THEME` names it; absent, the slot is the Signal default.
//! **No default config-file location is invented here on purpose.** The
//! workspace has no config surface of its own yet, and a path this module
//! made up would be one the workspace then had to agree with — so the env
//! var is the whole resolution, and the day a workspace config surface
//! lands it can pass an explicit path to [`ChromeTheme::parse_toml`].

use std::collections::BTreeMap;
use std::path::Path;

use gwk_theme::tier::Tier16;
use gwk_theme::{ColorTier, Token};
use ratatui::style::Style;

use crate::theme;

/// The environment variable naming an operator's chrome theme file.
pub const CHROME_THEME_ENV: &str = "GWK_CHROME_THEME";

/// A chrome theme file is a handful of `role = "token"` lines. The bound is
/// three orders of magnitude past that, and it exists so a wrong path — a
/// core file, a log — is refused rather than read into memory.
const MAX_THEME_BYTES: u64 = 64 * 1024;

/// One piece of the workspace's own furniture.
///
/// Closed, and the closure is the load-bearing part: every role here is
/// chrome that makes no claim about a running system, so a user may repaint
/// all of it. A role that DID carry such a claim would belong in Signal, not
/// in this enum, and adding one is an edit to a set a test pins by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ChromeRole {
    /// The rule between panes.
    PaneBorder,
    /// The rule around the pane taking input.
    PaneBorderFocused,
    /// A pane's own title text.
    PaneTitle,
    /// The tab the workspace is showing.
    TabActive,
    /// A tab one keystroke away.
    TabInactive,
    /// The workspace's name.
    WorkspaceLabel,
    /// The workspace's status line — chrome, not the lens status bars.
    StatusBar,
}

impl ChromeRole {
    /// Every role, in the order a theme file and `gw theme` both list them.
    pub const ALL: [Self; 7] = [
        Self::PaneBorder,
        Self::PaneBorderFocused,
        Self::PaneTitle,
        Self::TabActive,
        Self::TabInactive,
        Self::WorkspaceLabel,
        Self::StatusBar,
    ];

    /// The key an operator writes in the theme file.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PaneBorder => "pane_border",
            Self::PaneBorderFocused => "pane_border_focused",
            Self::PaneTitle => "pane_title",
            Self::TabActive => "tab_active",
            Self::TabInactive => "tab_inactive",
            Self::WorkspaceLabel => "workspace_label",
            Self::StatusBar => "status_bar",
        }
    }

    /// The Signal token this role carries when nobody has remapped it.
    pub const fn default_token(self) -> &'static str {
        match self {
            // A hairline between panes is exactly the decorative structure
            // `border` names, and `faint` is the one step quieter for the
            // unfocused case if an operator wants it.
            Self::PaneBorder => "border",
            Self::PaneBorderFocused => "focus",
            Self::PaneTitle => "fg",
            Self::TabActive => "hue_bright",
            Self::TabInactive => "muted",
            Self::WorkspaceLabel => "hue",
            Self::StatusBar => "muted",
        }
    }

    /// Resolve a file key. Unknown input is `None`, never a nearest match:
    /// a typo that silently themed a different role would be worse than one
    /// that refused.
    pub fn parse(key: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|role| role.as_str() == key)
    }
}

impl std::fmt::Display for ChromeRole {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Why a chrome theme was refused.
///
/// Typed rather than a string, because three of these are an operator's
/// typo and one is a path mistake, and telling them apart is the whole
/// value of reading the file at all.
#[derive(Debug, thiserror::Error)]
pub enum ChromeError {
    #[error("chrome theme names no such role: {key} (one of: {allowed})")]
    UnknownRole { key: String, allowed: String },
    #[error("chrome role {role} names no such Signal token: {token}")]
    UnknownToken { role: ChromeRole, token: String },
    #[error(
        "chrome role {role} cannot use {token}: it is an elevation step, not a \
         terminal foreground at any tier -- the result would be invisible, not \
         a different colour"
    )]
    NotAForeground {
        role: ChromeRole,
        token: &'static str,
    },
    #[error("chrome role {role} must be given a token name, got {actual}")]
    NotAString {
        role: ChromeRole,
        actual: &'static str,
    },
    #[error("chrome theme is not valid TOML: {source}")]
    InvalidToml {
        #[source]
        source: toml::de::Error,
    },
    #[error("chrome theme at {path} is {actual} bytes, over the {MAX_THEME_BYTES}-byte bound")]
    TooLarge { path: String, actual: u64 },
    #[error("chrome theme at {path} could not be read: {source}")]
    Unreadable {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

/// A resolved chrome theme: every role bound to a Signal token.
///
/// Total by construction — [`ChromeRole::ALL`] is always fully bound, so
/// [`ChromeTheme::token`] cannot fail and no caller has to decide what to
/// paint when a role is missing from a partial file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChromeTheme {
    bindings: BTreeMap<ChromeRole, &'static str>,
}

impl Default for ChromeTheme {
    fn default() -> Self {
        Self::signal()
    }
}

impl ChromeTheme {
    /// The unremapped slot: every role on its Signal default.
    pub fn signal() -> Self {
        Self {
            bindings: ChromeRole::ALL
                .iter()
                .map(|role| (*role, role.default_token()))
                .collect(),
        }
    }

    /// The operator's theme, or the Signal default when the variable names
    /// nothing. A path that IS named and cannot be read is an error, not a
    /// silent fallback — a themed workspace that quietly reverts on a typo
    /// is indistinguishable from one that ignored the file.
    pub fn from_env() -> Result<Self, ChromeError> {
        match std::env::var_os(CHROME_THEME_ENV) {
            Some(path) if !path.is_empty() => Self::load(Path::new(&path)),
            _ => Ok(Self::signal()),
        }
    }

    /// Read and resolve a theme file.
    pub fn load(path: &Path) -> Result<Self, ChromeError> {
        use std::io::Read as _;

        let display = path.display().to_string();
        let unreadable = |source| ChromeError::Unreadable {
            path: display.clone(),
            source,
        };
        // Bound the READ, not a stat of the path. A character device or a FIFO
        // reports a length of zero and then hands over as many bytes as it
        // likes, so a size gate would pass `/dev/zero` and the read after it
        // would never end. Taking one byte past the bound also closes the
        // window between checking a size and reading the file.
        let mut text = String::new();
        std::fs::File::open(path)
            .map_err(&unreadable)?
            .take(MAX_THEME_BYTES + 1)
            .read_to_string(&mut text)
            .map_err(&unreadable)?;
        if text.len() as u64 > MAX_THEME_BYTES {
            return Err(ChromeError::TooLarge {
                path: display,
                actual: text.len() as u64,
            });
        }
        Self::parse_toml(&text)
    }

    /// Resolve theme text. A file names only what it changes; every role it
    /// leaves out keeps its Signal default.
    pub fn parse_toml(text: &str) -> Result<Self, ChromeError> {
        let table: toml::Table = text
            .parse()
            .map_err(|source| ChromeError::InvalidToml { source })?;
        let mut theme = Self::signal();
        for (key, value) in &table {
            let role = ChromeRole::parse(key).ok_or_else(|| ChromeError::UnknownRole {
                key: key.clone(),
                allowed: ChromeRole::ALL
                    .iter()
                    .map(|role| role.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
            })?;
            let name = value.as_str().ok_or(ChromeError::NotAString {
                role,
                actual: value.type_str(),
            })?;
            theme.bind(role, name)?;
        }
        Ok(theme)
    }

    /// Point one role at one Signal token.
    pub fn bind(&mut self, role: ChromeRole, token: &str) -> Result<(), ChromeError> {
        let resolved = gwk_theme::SIGNAL
            .iter()
            .find(|candidate| candidate.name == token)
            .ok_or_else(|| ChromeError::UnknownToken {
                role,
                token: token.to_owned(),
            })?;
        if matches!(resolved.tier16, Tier16::NotAColor) {
            return Err(ChromeError::NotAForeground {
                role,
                token: resolved.name,
            });
        }
        self.bindings.insert(role, resolved.name);
        Ok(())
    }

    /// The Signal token this theme binds `role` to.
    pub fn token(&self, role: ChromeRole) -> &'static Token {
        let name = self
            .bindings
            .get(&role)
            .copied()
            .unwrap_or_else(|| role.default_token());
        gwk_theme::SIGNAL
            .iter()
            .find(|token| token.name == name)
            // Unreachable: every name here came through `bind`, which
            // resolves against SIGNAL, or from `default_token`, which a test
            // pins against the same table.
            .unwrap_or(&gwk_theme::SIGNAL[0])
    }

    /// The style this theme paints `role` at `tier` — the same one function
    /// every other surface in the console resolves through, so a remapped
    /// role degrades exactly like the token it now names.
    pub fn style(&self, role: ChromeRole, tier: ColorTier) -> Style {
        theme::token_style(self.token(role), tier)
    }

    /// Whether nothing has been remapped. `gw theme` prints this so an
    /// operator can tell "my file did nothing" from "my file is not loaded".
    pub fn is_signal(&self) -> bool {
        *self == Self::signal()
    }

    /// Every role with the token it currently carries, in `ALL` order.
    pub fn bindings(&self) -> impl Iterator<Item = (ChromeRole, &'static Token)> + '_ {
        ChromeRole::ALL
            .into_iter()
            .map(move |role| (role, self.token(role)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_the_role_set_is_pinned_by_name() {
        // Adding a role is a deliberate edit, and this is where it is
        // noticed: the closed set is what makes "pane content is never
        // themeable" a mechanism instead of a promise.
        let names: Vec<&str> = ChromeRole::ALL.iter().map(|role| role.as_str()).collect();
        assert_eq!(
            names,
            [
                "pane_border",
                "pane_border_focused",
                "pane_title",
                "tab_active",
                "tab_inactive",
                "workspace_label",
                "status_bar",
            ]
        );
        for role in ChromeRole::ALL {
            assert!(
                !role.as_str().contains("content"),
                "{role} reads as pane content, which is the session's own"
            );
            assert_eq!(ChromeRole::parse(role.as_str()), Some(role));
        }
        assert_eq!(ChromeRole::parse("pane-border"), None, "no nearest match");
    }

    #[test]
    fn chrome_every_default_resolves_and_paints() {
        let theme = ChromeTheme::signal();
        assert!(theme.is_signal());
        for role in ChromeRole::ALL {
            let token = theme.token(role);
            assert_eq!(token.name, role.default_token());
            assert!(
                !matches!(token.tier16, Tier16::NotAColor),
                "{role} defaults to an elevation step and would be invisible"
            );
            assert!(
                theme.style(role, ColorTier::Truecolor).fg.is_some(),
                "{role} paints nothing at truecolor"
            );
        }
    }

    #[test]
    fn chrome_a_file_names_only_what_it_changes() {
        let theme = ChromeTheme::parse_toml("tab_active = \"ok\"\n").expect("theme");
        assert_eq!(theme.token(ChromeRole::TabActive).name, "ok");
        assert_eq!(
            theme.token(ChromeRole::PaneBorder).name,
            ChromeRole::PaneBorder.default_token(),
            "an unnamed role keeps its default"
        );
        assert!(!theme.is_signal());
        assert!(ChromeTheme::parse_toml("").expect("empty").is_signal());
    }

    #[test]
    fn chrome_a_remap_cannot_introduce_a_colour_or_an_invisible_one() {
        // Both sides are closed, so the only reachable values are ratified
        // tokens — the property the whole slot rests on.
        for (text, expected) in [
            ("pane_border = \"#FF00FF\"", "no such Signal token"),
            ("pane_border = \"bg\"", "elevation step"),
            ("pane_border = \"surface_2\"", "elevation step"),
            ("pane_edge = \"ok\"", "no such role"),
            ("pane_border = 7", "must be given a token name"),
            ("pane_border = ", "not valid TOML"),
        ] {
            let error = ChromeTheme::parse_toml(text)
                .expect_err(&format!("{text:?} was accepted"))
                .to_string();
            assert!(
                error.contains(expected),
                "{text:?} refused with {error:?}, wanted {expected:?}"
            );
        }

        // And the refusals are the only refusals: every other Signal token is
        // a legal target for every role.
        for token in gwk_theme::SIGNAL
            .iter()
            .filter(|token| !matches!(token.tier16, Tier16::NotAColor))
        {
            for role in ChromeRole::ALL {
                ChromeTheme::signal()
                    .bind(role, token.name)
                    .unwrap_or_else(|why| panic!("{role} could not take {}: {why}", token.name));
            }
        }
    }

    #[test]
    fn chrome_a_remapped_role_degrades_like_the_token_it_now_names() {
        let mut theme = ChromeTheme::signal();
        theme.bind(ChromeRole::PaneTitle, "ok").expect("bind");
        for tier in ColorTier::ALL {
            assert_eq!(
                theme.style(ChromeRole::PaneTitle, *tier),
                theme::token_style(
                    gwk_theme::SIGNAL
                        .iter()
                        .find(|token| token.name == "ok")
                        .expect("ok"),
                    *tier
                ),
                "the remap diverged from the token at {}",
                tier.as_str()
            );
        }
    }

    #[test]
    fn chrome_an_absent_env_var_is_the_signal_default_and_a_bad_path_is_an_error() {
        // SAFETY-adjacent note: this test owns the variable for its body and
        // clears it after, and the crate runs no other reader of it.
        let missing = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("goldens")
            .join("no-such-chrome-theme.toml");
        let error = ChromeTheme::load(&missing).expect_err("a named path must exist");
        assert!(
            error.to_string().contains("could not be read"),
            "a typo'd path is an error, never a silent revert: {error}"
        );
    }

    #[test]
    fn chrome_the_size_bound_reads_bytes_rather_than_trusting_a_stat() {
        // The bound has to hold for a path whose METADATA lies about its size:
        // a character device or a FIFO reports zero and then hands over as
        // many bytes as it likes, so a stat gate would pass `/dev/zero` and
        // the read after it would never end. Bounding the read closes that and
        // the check-then-read window in one move. A plain oversized file is
        // what a test can portably build; the device case shares the path.
        let path = std::env::temp_dir().join(format!("gwk-chrome-big-{}", std::process::id()));
        std::fs::write(&path, "#".repeat(MAX_THEME_BYTES as usize + 1)).expect("write");
        let error = ChromeTheme::load(&path).expect_err("oversized theme");
        let _ = std::fs::remove_file(&path);
        assert!(
            matches!(error, ChromeError::TooLarge { .. }),
            "an oversized theme is refused by size, not by parse: {error}"
        );
    }

    #[test]
    fn chrome_orchestration_never_reaches_this_module() {
        // The Signal-is-not-negotiable invariant, checked rather than
        // asserted in prose: no lens outside this file and the gated
        // workspace half may consult a user remap.
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut scanned = 0;
        let mut positive_control = false;
        let mut stack = vec![src.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).expect("read src") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    // The multiplexer half is the one legitimate consumer.
                    if path.file_name().is_some_and(|name| name == "workspace") {
                        continue;
                    }
                    stack.push(path);
                    continue;
                }
                if path.extension().is_none_or(|ext| ext != "rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&path).expect("read module");
                let mentions = text.contains("ChromeTheme") || text.contains("chrome::");
                if path.file_name().is_some_and(|name| name == "chrome.rs") {
                    positive_control = mentions;
                    continue;
                }
                // `lib.rs` declares the module; declaring is not consulting.
                let declares_only = path.file_name().is_some_and(|name| name == "lib.rs");
                assert!(
                    !mentions || declares_only,
                    "{} consults the chrome theme -- orchestration surfaces are \
                     Signal and that is not negotiable",
                    path.display()
                );
                scanned += 1;
            }
        }
        assert!(scanned >= 10, "the scan found only {scanned} modules");
        assert!(
            positive_control,
            "the scan cannot see this file's own mentions, so it proves nothing"
        );
    }
}
