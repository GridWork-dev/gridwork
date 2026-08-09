//! Pure column arithmetic and extended grapheme-boundary primitives.
//!
//! This crate accepts UTF-8 text and returns values. It performs no I/O, spawns
//! no process, and emits or parses no terminal-protocol bytes.
//!
//! # Width tailoring
//!
//! [`cluster_width`] uses Unicode Standard Annex #11 as a *basis for explicit
//! tailoring*, not as an off-the-shelf terminal-emulator answer. UAX #11 says
//! its `East_Asian_Width` property needs case-by-case tailoring for modern
//! terminals. GridWork resolves `Wide` and `Fullwidth` to two columns, defaults
//! `Ambiguous` to one unless an East Asian context is explicitly selected,
//! keeps nonspacing marks with their base, and treats `Emoji_Presentation` as
//! wide except for `Regional_Indicator`. Standardized variation selectors
//! affect the result only when the caller says its font honors them.
//!
//! Width is always measured on an extended grapheme cluster. Summing codepoint
//! widths is deliberately not an API here: marks do not advance independently,
//! and ligatures such as emoji ZWJ sequences are one editing and width unit.
//!
//! [`grapheme_boundaries`] follows the default extended-cluster rules in Unicode
//! Standard Annex #29. Its byte offsets are suitable for cursor movement,
//! deletion, and selection, including degenerate input such as an isolated
//! combining mark or control character.

#![doc(html_root_url = "https://docs.rs/gwk-text")]

use std::borrow::Cow;

use unicode_properties::{EmojiStatus, UnicodeEmoji};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const TEXT_PRESENTATION_SELECTOR: char = '\u{fe0e}';
const EMOJI_PRESENTATION_SELECTOR: char = '\u{fe0f}';

/// How characters with `East_Asian_Width=Ambiguous` are resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AmbiguousWidth {
    /// One column. This is the fallback when context cannot be established.
    #[default]
    Narrow,
    /// Two columns in an explicitly established East Asian context.
    Wide,
}

/// The caller-supplied context needed to tailor cluster width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WidthProfile {
    ambiguous_width: AmbiguousWidth,
    honor_variation_selectors: bool,
}

impl WidthProfile {
    /// A profile for an explicitly established East Asian context.
    pub const fn east_asian() -> Self {
        Self {
            ambiguous_width: AmbiguousWidth::Wide,
            honor_variation_selectors: true,
        }
    }

    /// Ignore VS15/VS16 when the selected font does not honor them.
    pub const fn without_variation_selectors(mut self) -> Self {
        self.honor_variation_selectors = false;
        self
    }

    /// The profile's resolution for ambiguous characters.
    pub const fn ambiguous_width(self) -> AmbiguousWidth {
        self.ambiguous_width
    }

    /// Whether standardized text/emoji selectors may override default width.
    pub const fn honors_variation_selectors(self) -> bool {
        self.honor_variation_selectors
    }
}

impl Default for WidthProfile {
    fn default() -> Self {
        Self {
            ambiguous_width: AmbiguousWidth::Narrow,
            honor_variation_selectors: true,
        }
    }
}

/// Return every extended grapheme-cluster boundary as a UTF-8 byte offset.
///
/// The list always starts at zero and ends at `text.len()`. Empty input returns
/// `[0]`; a lone combining mark or control character returns one cluster.
pub fn grapheme_boundaries(text: &str) -> Vec<usize> {
    let mut boundaries: Vec<usize> = text
        .grapheme_indices(true)
        .map(|(offset, _)| offset)
        .collect();
    if boundaries.is_empty() {
        boundaries.push(0);
    }
    if boundaries.last().copied() != Some(text.len()) {
        boundaries.push(text.len());
    }
    boundaries
}

/// Resolve one extended grapheme cluster to its predicted column width.
///
/// Callers normally obtain `cluster` by slicing at [`grapheme_boundaries`].
/// Passing more than one cluster is deterministic, but [`text_width`] is the
/// intended API for whole strings.
pub fn cluster_width(cluster: &str, profile: WidthProfile) -> usize {
    let measured = if profile.honor_variation_selectors {
        Cow::Borrowed(cluster)
    } else {
        let without_selectors: String = cluster
            .chars()
            .filter(|character| {
                !matches!(
                    *character,
                    TEXT_PRESENTATION_SELECTOR | EMOJI_PRESENTATION_SELECTOR
                )
            })
            .collect();
        if without_selectors.len() == cluster.len() {
            Cow::Borrowed(cluster)
        } else {
            Cow::Owned(without_selectors)
        }
    };

    let width = match profile.ambiguous_width {
        AmbiguousWidth::Narrow => UnicodeWidthStr::width(measured.as_ref()),
        AmbiguousWidth::Wide => UnicodeWidthStr::width_cjk(measured.as_ref()),
    };

    if profile.honor_variation_selectors
        && (cluster.contains(TEXT_PRESENTATION_SELECTOR)
            || cluster.contains(EMOJI_PRESENTATION_SELECTOR))
    {
        return width;
    }

    if measured
        .chars()
        .any(|character| has_emoji_presentation(character) && !is_regional_indicator(character))
    {
        width.max(2)
    } else {
        width
    }
}

/// Resolve a string by segmenting it first, then summing cluster widths.
pub fn text_width(text: &str, profile: WidthProfile) -> usize {
    text.graphemes(true)
        .map(|cluster| cluster_width(cluster, profile))
        .sum()
}

fn has_emoji_presentation(character: char) -> bool {
    matches!(
        character.emoji_status(),
        EmojiStatus::EmojiPresentation
            | EmojiStatus::EmojiPresentationAndModifierBase
            | EmojiStatus::EmojiPresentationAndEmojiComponent
            | EmojiStatus::EmojiPresentationAndModifierAndEmojiComponent
    )
}

fn is_regional_indicator(character: char) -> bool {
    unicode_properties::emoji::is_regional_indicator(character)
}
