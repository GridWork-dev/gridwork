//! Deterministic precedence, and what happens when it does not decide.
//!
//! ADR-0032 D5 fixes the tier order and one rule about ties: **equal-authority
//! conflicts fail closed.** Both halves matter, and the second is the one that
//! is easy to get quietly wrong — a resolver that picks "the first one" when
//! two equal sources disagree still returns a manifest, still renders, and is
//! silently arbitrary. Whichever source happened to be enumerated first
//! becomes policy, and nothing in the output says so.
//!
//! So [`resolve`] distinguishes three outcomes, not two:
//!
//! - `Ok(None)` — nothing spoke to this decision.
//! - `Ok(Some(i))` — one tier won outright.
//! - `Err(_)` — the top tier disagreed with itself.
//!
//! The first and third are the ones that collapse if you are careless. "Nobody
//! set this" and "everybody set it to a different thing" are both *not a
//! value*, and a resolver returning `Option` alone reports them identically.

use crate::participation::ParticipationReason;

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

impl PrecedenceTier {
    /// Every tier, highest authority first.
    pub const ALL: [Self; 6] = [
        Self::Security,
        Self::RunDeclaration,
        Self::RouteConfig,
        Self::RequestedSkill,
        Self::AutomaticSkill,
        Self::Annotation,
    ];

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

/// Resolve one decision from every tier that spoke to it.
///
/// Returns the index of the winning contribution so the caller keeps whatever
/// identity it already has for each source — this crate does not need to
/// invent one, and fork F11 (actor identity) is not ruled yet.
///
/// Equal values at the same top tier are not a conflict: two sources agreeing
/// is agreement, however many of them there are.
pub fn resolve<T: PartialEq>(
    contributions: &[Contribution<T>],
) -> Result<Option<usize>, PrecedenceConflict> {
    // The COUNT of top-tier contributions decides, never a running "best" that
    // an empty list and a real winner would both leave untouched. Same defect
    // class as a fold that cannot tell "summed to zero" from "summed over
    // nothing" — by the time you look at the accumulator, they are identical.
    let Some(top) = contributions.iter().map(|c| c.tier).min() else {
        return Ok(None);
    };

    let mut winner = None;
    let mut distinct = 0usize;
    for (index, contribution) in contributions.iter().enumerate() {
        if contribution.tier != top {
            continue;
        }
        match winner {
            None => {
                winner = Some(index);
                distinct = 1;
            }
            Some(first) => {
                // Compare against every value already counted distinct, not
                // just the first: three sources offering A, B, B is two
                // distinct values, and reporting three would overstate the
                // disagreement in the operator-facing message.
                //
                // The range is EXCLUSIVE of `index`. Inclusive, the candidate
                // finds itself, `already_seen` is unconditionally true, and
                // the count never leaves 1 — which reads as unanimity and
                // returns a winner instead of failing closed.
                let already_seen = contributions[first..index]
                    .iter()
                    .filter(|c| c.tier == top)
                    .any(|c| c.value == contribution.value);
                if !already_seen {
                    distinct += 1;
                }
            }
        }
    }

    if distinct > 1 {
        return Err(PrecedenceConflict {
            tier: top,
            distinct_values: distinct,
        });
    }
    Ok(winner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(tier: PrecedenceTier, value: &str) -> Contribution<String> {
        Contribution::new(tier, value.to_owned())
    }

    #[test]
    fn nothing_spoke_is_not_the_same_as_nothing_won() {
        // The distinction the whole three-outcome signature exists for.
        assert_eq!(resolve::<String>(&[]), Ok(None));
        let one = [c(PrecedenceTier::Annotation, "x")];
        assert_eq!(resolve(&one), Ok(Some(0)));
    }

    #[test]
    fn the_highest_tier_wins_regardless_of_order() {
        let contributions = [
            c(PrecedenceTier::Annotation, "advisory"),
            c(PrecedenceTier::Security, "mandated"),
            c(PrecedenceTier::RouteConfig, "configured"),
        ];
        let index = resolve(&contributions)
            .expect("no conflict")
            .expect("a winner");
        assert_eq!(contributions[index].value, "mandated");
    }

    #[test]
    fn equal_authority_disagreement_fails_closed() {
        let contributions = [
            c(PrecedenceTier::RouteConfig, "left"),
            c(PrecedenceTier::RouteConfig, "right"),
            c(PrecedenceTier::Annotation, "ignored"),
        ];
        let err = resolve(&contributions).expect_err("must fail closed");
        assert_eq!(err.tier, PrecedenceTier::RouteConfig);
        assert_eq!(err.distinct_values, 2);
        assert_eq!(err.reason(), ParticipationReason::PrecedenceLoss);
    }

    #[test]
    fn equal_authority_agreement_is_not_a_conflict() {
        let contributions = [
            c(PrecedenceTier::Security, "same"),
            c(PrecedenceTier::Security, "same"),
        ];
        assert_eq!(resolve(&contributions), Ok(Some(0)));
    }

    #[test]
    fn a_lower_tier_disagreeing_with_itself_is_irrelevant() {
        // Only the TOP tier's agreement matters — lower tiers already lost,
        // and a resolver that scanned all tiers for conflicts would fail
        // closed on inputs that precedence had already decided.
        let contributions = [
            c(PrecedenceTier::Security, "wins"),
            c(PrecedenceTier::Annotation, "left"),
            c(PrecedenceTier::Annotation, "right"),
        ];
        let index = resolve(&contributions)
            .expect("no conflict")
            .expect("a winner");
        assert_eq!(contributions[index].value, "wins");
    }

    #[test]
    fn distinct_count_reports_values_not_contributors() {
        let contributions = [
            c(PrecedenceTier::RunDeclaration, "a"),
            c(PrecedenceTier::RunDeclaration, "b"),
            c(PrecedenceTier::RunDeclaration, "b"),
        ];
        let err = resolve(&contributions).expect_err("must fail closed");
        assert_eq!(err.distinct_values, 2, "three contributors, two values");
    }

    #[test]
    fn tier_order_is_d5_order_and_outranks_reads_forward() {
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
