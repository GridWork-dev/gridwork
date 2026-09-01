//! The D5 resolver, and what happens when it does not decide.
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
//!
//! The types are `gwk-context`'s; only the implementation moved here (see the
//! crate docs for why).

use gwk_context::{Contribution, PrecedenceConflict};

/// Resolve one decision from every tier that spoke to it.
///
/// Returns the index of the winning contribution so the caller keeps whatever
/// identity it already has for each source — this crate does not need to
/// invent one.
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
    use gwk_context::{ParticipationReason, PrecedenceTier};

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
    fn a_lower_tier_between_two_top_tier_sources_cannot_mask_their_disagreement() {
        // The only input that can tell whether the `already_seen` scan filters
        // to the top tier. Unfiltered, the Annotation at index 1 carries the
        // later Security value, the scan reports "allow" as already seen,
        // `distinct` never leaves 1, and two Security sources that disagree
        // are elected as though they had agreed — a D5 fail-OPEN.
        //
        // Unreachable through `compile`, which sorts into canonical order and
        // so makes the top tier a contiguous prefix. It is reachable here
        // because `resolve` is public API whose stated contract is
        // order-independence (`the_highest_tier_wins_regardless_of_order`),
        // which puts unordered input squarely in-contract.
        let contributions = [
            c(PrecedenceTier::Security, "deny"),
            c(PrecedenceTier::Annotation, "allow"),
            c(PrecedenceTier::Security, "allow"),
        ];
        let err = resolve(&contributions).expect_err("must fail closed");
        assert_eq!(err.tier, PrecedenceTier::Security);
        assert_eq!(err.distinct_values, 2);
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
}
