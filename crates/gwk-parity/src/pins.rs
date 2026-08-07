//! The version pins and the `D` bound, both straight out of
//! `docs/PARITY.md`.
//!
//! Neither is a number this crate is allowed to just agree with by eye:
//! [`d_bound`] is computed from `gwk_domain`'s own constant so a change to
//! the kernel's poll ceiling moves it here for free, and the three CLI pins
//! are asserted against the literal doc text below so an edit to one side
//! without the other fails the build, not a review.

use std::time::Duration;

/// **D** = `SUBSCRIPTION_POLL_SECS` + 5 seconds, per `docs/PARITY.md`'s "The
/// bound: D" — the kernel's own subscription-poll ceiling, plus a fixed
/// margin for adapter processing and engine event latency. Read from the
/// constant, never written as a literal, so the day the poll ceiling moves,
/// this moves with it.
pub fn d_bound() -> Duration {
    Duration::from_secs(gwk_domain::protocol::SUBSCRIPTION_POLL_SECS + 5)
}

/// `docs/PARITY.md`'s "Version pins" table, "CLI reports" column, Claude
/// Code row — everything after the version number is the CLI's own
/// parenthetical, not part of the pin this crate checks against.
pub const CLAUDE_CLI_VERSION: &str = "2.1.223";

/// `docs/PARITY.md`'s "Version pins" table, Codex row.
pub const CODEX_CLI_VERSION: &str = "0.146.0";

/// `docs/PARITY.md`'s "Version pins" table, opencode row.
pub const OPENCODE_CLI_VERSION: &str = "1.18.3";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn d_bound_is_derived_from_the_kernel_constant_and_is_ten_seconds_today() {
        assert_eq!(
            d_bound(),
            Duration::from_secs(gwk_domain::protocol::SUBSCRIPTION_POLL_SECS + 5),
            "d_bound() must track the constant, not restate it"
        );
        assert_eq!(d_bound(), Duration::from_secs(10));
    }

    /// A drift guard: if either this file's pins or `docs/PARITY.md`'s table
    /// changes without the other, this fails the build rather than the
    /// harness silently judging an unpinned engine build.
    #[test]
    fn version_pins_match_the_literal_text_of_docs_parity_md() {
        const PARITY_MD: &str = include_str!("../../../docs/PARITY.md");
        assert!(
            PARITY_MD.contains(CLAUDE_CLI_VERSION),
            "docs/PARITY.md no longer contains the pinned Claude Code version {CLAUDE_CLI_VERSION}"
        );
        assert!(
            PARITY_MD.contains(CODEX_CLI_VERSION),
            "docs/PARITY.md no longer contains the pinned Codex version {CODEX_CLI_VERSION}"
        );
        assert!(
            PARITY_MD.contains(OPENCODE_CLI_VERSION),
            "docs/PARITY.md no longer contains the pinned opencode version {OPENCODE_CLI_VERSION}"
        );
    }
}
