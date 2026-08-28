//! The named stages of Context truth.
//!
//! **This enum is for naming, not for tagging.** No record carries a stage
//! field. Which stage a record belongs to is which table it is in and which
//! endpoint produced it — that is ruling R4 (fork F4), and it follows the
//! codebase, where no entity carries a cross-cutting "which axis" tag and
//! adding the first one here would be a change of kind, not of degree.
//!
//! A stage enum that exists is exactly the thing someone later adds as a
//! column, so the rule is stated at the type rather than in a plan nobody
//! reads at the moment of temptation.
//!
//! What it IS for: lens tabs, CLI output, error text, and Explain/Compare
//! labels — the places where five files would otherwise spell "released" five
//! ways.
//!
//! ## Why five and not three
//!
//! ADR-0032 D4 names three truth levels (declared / resolved / observed). D3's
//! dispatch pipeline and D4's own supplement list separately name a *release*
//! at render and a *finalization* at the end. The ADR never reconciles this.
//!
//! R4 settles it at five rather than inheriting the ambiguity, because the
//! alternative is Explain/Compare and the 8F lens each guessing what "truth
//! level" means and the guess hardening into the wire contract. If a later
//! reading collapses two of these, that is a contract change made on purpose.

use crate::participation::closed_token_enum;

closed_token_enum! {
    /// The five named stages a Context record can belong to.
    ///
    /// Ordered as the pipeline runs, so `PartialOrd` means "no later than".
    #[derive(
        Debug,
        Clone,
        Copy,
        PartialEq,
        Eq,
        PartialOrd,
        Ord,
        Hash,
        serde::Serialize,
        serde::Deserialize,
        specta::Type,
    )]
    #[serde(rename_all = "snake_case")]
    pub enum ContextStage {
        /// What was asked for, before resolution: run declarations, requested
        /// skills, route hints. Not yet authority-checked.
        Declared,
        /// The immutable compiled manifest for one spawn attempt — the single
        /// artifact the verifier checks and the adapter renders from.
        Resolved,
        /// The append-only release supplement, written exactly once at render.
        Released,
        /// Zero or more observation supplements written while the attempt runs.
        Observed,
        /// The one finalization supplement, written when the attempt settles.
        Finalized,
    }
}

impl ContextStage {
    /// The wire/display token. Matches the serde representation exactly —
    /// one spelling, whether a stage is being written to a log or drawn in a
    /// lens tab.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Declared => "declared",
            Self::Resolved => "resolved",
            Self::Released => "released",
            Self::Observed => "observed",
            Self::Finalized => "finalized",
        }
    }

    /// True once the manifest is immutable — from `Resolved` onward.
    ///
    /// `Declared` is the only stage whose content can still change, because it
    /// is the only one that has not been through authority resolution.
    pub fn is_immutable(self) -> bool {
        self != Self::Declared
    }
}

impl std::fmt::Display for ContextStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_every_variant() {
        // ALL is macro-derived from the enum's own declaration, so it cannot
        // fall behind it; what this match guards is `as_str` — a sixth stage
        // fails to compile here until it gets a spelling, and the count pins
        // the list against R4's externally documented five.
        for stage in ContextStage::ALL {
            let named = match stage {
                ContextStage::Declared => "declared",
                ContextStage::Resolved => "resolved",
                ContextStage::Released => "released",
                ContextStage::Observed => "observed",
                ContextStage::Finalized => "finalized",
            };
            assert_eq!(stage.as_str(), named);
        }
        assert_eq!(ContextStage::ALL.len(), 5);
    }

    #[test]
    fn display_and_serde_agree_on_one_spelling() {
        // The whole reason this enum exists is that a lens tab and a log entry
        // must not disagree. Assert it rather than trusting rename_all.
        for stage in ContextStage::ALL {
            let json = serde_json::to_string(&stage).expect("serializable");
            assert_eq!(json, format!("\"{stage}\""));
        }
    }

    #[test]
    fn pipeline_order_is_the_declared_order() {
        assert!(ContextStage::Declared < ContextStage::Resolved);
        assert!(ContextStage::Resolved < ContextStage::Released);
        assert!(ContextStage::Released < ContextStage::Observed);
        assert!(ContextStage::Observed < ContextStage::Finalized);
    }

    #[test]
    fn only_declared_is_mutable() {
        assert!(!ContextStage::Declared.is_immutable());
        for stage in ContextStage::ALL.into_iter().skip(1) {
            assert!(stage.is_immutable(), "{stage} should be immutable");
        }
    }
}
