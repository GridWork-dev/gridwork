//! The precedence vocabulary: tiers, contributions, and the conflict a
//! resolver reports when the top tier does not agree with itself.
//!
//! ADR-0032 D5 fixes the tier order and one rule about ties: **equal-authority
//! conflicts fail closed.** The types here are what both halves of that rule
//! are spoken in. The resolver that applies them — the function that answers
//! "nothing spoke", "one tier won", or a [`PrecedenceConflict`] — lives in
//! `gwk-context-compiler`, not here. That is deliberate (R15): the verifier is
//! a separate crate precisely so its dependency graph cannot reach the
//! compiler's precedence implementation, and a resolver exported from the one
//! crate the verifier may depend on would have sat one `use` away under
//! whichever spelling a name-match guard did not know.

use gwk_domain::{ParticipationReason, closed_token_enum};

closed_token_enum! {
    /// The precedence tiers, highest authority first (ADR-0032 D5).
    ///
    /// `Ord` runs highest-authority-first: `Security < RunDeclaration` means
    /// security **wins**. That inversion is deliberate and load-bearing — the
    /// derived ordering follows declaration order, so the enum reads top-down in
    /// the same order D5 states it, and `min()` selects the winner.
    #[derive(
        Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
    )]
    #[serde(rename_all = "snake_case")]
    pub enum PrecedenceTier {
        /// Security and authority resolution. Never overridable: context may
        /// narrow what authority granted, never widen it (D3).
        Security,
        /// Explicit declarations on the run itself.
        RunDeclaration,
        /// Project, route, role, and capability configuration.
        RouteConfig,
        /// Skills the request explicitly asked for, in verified trust state.
        RequestedSkill,
        /// Skills the compiler selected on its own.
        AutomaticSkill,
        /// Memory, knowledge, graph, eval, and optimization annotations — advisory
        /// input, lowest authority.
        Annotation,
    }
}

impl PrecedenceTier {
    /// True if `self` outranks `other`.
    ///
    /// Spelled out rather than left to `<` at call sites, because the ordering
    /// is inverted and a reader who has not read this file will guess wrong.
    pub fn outranks(self, other: Self) -> bool {
        self < other
    }
}

/// One tier's input to a single decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contribution<T> {
    pub tier: PrecedenceTier,
    pub value: T,
}

impl<T> Contribution<T> {
    pub fn new(tier: PrecedenceTier, value: T) -> Self {
        Self { tier, value }
    }
}

/// The top tier disagreed with itself, so nothing was decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrecedenceConflict {
    /// The tier that failed to agree.
    pub tier: PrecedenceTier,
    /// How many distinct values it offered — always at least two.
    pub distinct_values: usize,
}

impl PrecedenceConflict {
    /// The participation reason a candidate dropped by this conflict carries.
    ///
    /// Every loser in a precedence decision — including one lost to a tie —
    /// is a `PrecedenceLoss`; the conflict is what the *detail* explains.
    pub fn reason(&self) -> ParticipationReason {
        ParticipationReason::PrecedenceLoss
    }
}

impl std::fmt::Display for PrecedenceConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:?} supplied {} conflicting values at equal authority; failing closed",
            self.tier, self.distinct_values
        )
    }
}

impl std::error::Error for PrecedenceConflict {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_conflict_names_its_tier_and_carries_the_loss_reason() {
        let conflict = PrecedenceConflict {
            tier: PrecedenceTier::RouteConfig,
            distinct_values: 2,
        };
        assert_eq!(conflict.reason(), ParticipationReason::PrecedenceLoss);
        assert!(conflict.to_string().contains("RouteConfig"), "{conflict}");
        assert!(
            conflict.to_string().contains("failing closed"),
            "{conflict}"
        );
    }

    #[test]
    fn tier_order_is_d5_order_and_outranks_reads_forward() {
        // ALL is macro-derived from the enum's declaration, so this count is a
        // real growth guard against D5's six documented tiers, not the
        // `[Self; 6].len() == 6` tautology it used to be.
        assert_eq!(PrecedenceTier::ALL.len(), 6);
        for pair in PrecedenceTier::ALL.windows(2) {
            assert!(
                pair[0].outranks(pair[1]),
                "{:?} should outrank {:?}",
                pair[0],
                pair[1]
            );
        }
        // The inversion, asserted directly: highest authority is the MINIMUM.
        assert_eq!(
            PrecedenceTier::ALL.iter().min(),
            Some(&PrecedenceTier::Security)
        );
    }
}
