//! Why a candidate did or did not take part in one compiled manifest.
//!
//! Two closed enums and a record. Both enums are closed on purpose and for
//! different reasons:
//!
//! - [`ParticipationState`] is a plain enum with no transition table (ruling
//!   R5 / fork F6). The resolved manifest is immutable, so participation is
//!   one compile-time classification, never a state walked across writes.
//!   `StateMachine`, EDGES, and ESCAPE exist for `TaskState` and
//!   `AttemptState`, which mutate; bolting that machinery onto a write-once
//!   value would describe transitions that cannot occur.
//! - [`ParticipationReason`] is closed because Explain/Compare's entire job is
//!   grouping, filtering, and branching on reason across thousands of
//!   manifests (ruling R6 / fork F3). The `Gate.kind` open-string idiom works
//!   only because nothing branches on `Gate.kind` programmatically. The true
//!   analogs are `KernelErrorCode`, `AppendError`, and `BlobError` — the
//!   existing closed refusal sets.

use crate::digest::Digest;

// One list per closed enum, in the `context_event_names!` / `context_aggregates!`
// shape from `wire.rs`, and for the same reason those macros exist: `ALL` as a
// second array literal is a list that can fall behind the enum, and
// `ALL.len()` on a `[Self; N]` is `N` BY TYPE — the assertion that was supposed
// to notice the drift compiled to `N == N` and could never fire. Deriving both
// the enum and `ALL` from one token list is the only version of "these agree"
// that does not depend on two places being edited together; the `.len()`
// assertions in the tests then pin the list against the externally documented
// count, which growth CAN fail.
macro_rules! closed_token_enum {
    (
        $(#[$attr:meta])*
        pub enum $name:ident {
            $($(#[$vdoc:meta])* $variant:ident),* $(,)?
        }
    ) => {
        $(#[$attr])*
        pub enum $name {
            $($(#[$vdoc])* $variant,)*
        }

        impl $name {
            /// Every variant, in declaration order. Derived from the one list
            /// that also declares the enum, so it cannot fall behind it.
            pub const ALL: [Self; [$(stringify!($variant)),*].len()] =
                [$(Self::$variant),*];
        }
    };
}
pub(crate) use closed_token_enum;

closed_token_enum! {
    /// Whether a candidate participated, and how far it got.
    ///
    /// Per ADR-0032 D5. Not a state machine: see the module docs.
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
    pub enum ParticipationState {
        /// Known to the catalog and admissible, but not eligible for this route.
        Available,
        /// Eligible for this route and offered to the compiler.
        Selectable,
        /// Selected into the resolved manifest.
        Active,
        /// Eligible, but the compiler ruled it out — always carries a reason.
        Excluded,
        /// Could not be considered at all: unreadable, unverified, or absent.
        Unavailable,
    }
}

impl ParticipationState {
    /// True for the two states that owe an explanation.
    ///
    /// `Excluded` and `Unavailable` are the answers an operator will ask
    /// "why?" about; the other three are self-explaining.
    pub fn requires_reason(self) -> bool {
        matches!(self, Self::Excluded | Self::Unavailable)
    }
}

closed_token_enum! {
    /// The closed set of reasons a candidate did not become `Active`.
    ///
    /// A new reason is a contract change with a new binding, never a string a
    /// caller invents — the same rule `KernelCommand` states for itself.
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
    pub enum ParticipationReason {
        /// A higher precedence tier supplied a conflicting value (D5).
        PrecedenceLoss,
        /// Authority resolution excluded it. Upstream of context compilation and
        /// never overridable by it — context may narrow, never widen (D3).
        PermissionDenied,
        /// Dropped to stay inside a declared budget.
        BudgetCut,
        /// Third-party material still in its initial quarantined trust state (D5).
        Quarantined,
        /// Reviewed and rejected for this exact digest.
        Rejected,
        /// The verified pin does not match the digest actually found — an upstream
        /// change mints a new quarantined candidate, never a silent update (D5).
        PinDrift,
        /// The route, role, or capability does not admit it.
        NotEligible,
        /// The source could not be read at all.
        Unavailable,
    }
}

/// One candidate's participation record inside a resolved manifest.
///
/// `deny_unknown_fields` because a field this record does not know is either a
/// version skew or a caller inventing contract, and both should be loud.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct Participation {
    /// What the candidate was, by content — not by name, which can be reused.
    pub digest: Digest,
    pub state: ParticipationState,
    /// Present exactly when [`ParticipationState::requires_reason`] is true.
    /// Enforced by [`Participation::validate`], not by the type, so a record
    /// off the wire can be inspected before it is rejected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub reason: Option<ParticipationReason>,
    /// Free text that narrows the reason for a human. Never parsed, never
    /// branched on — that is what the closed enum above is for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub detail: Option<String>,
}

/// Why a participation record is not internally consistent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParticipationError {
    /// `Excluded`/`Unavailable` without a reason — an unexplained refusal.
    MissingReason,
    /// A reason on a state that did not refuse anything.
    UnexpectedReason,
    /// Detail with no reason to narrow.
    DetailWithoutReason,
}

impl std::fmt::Display for ParticipationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::MissingReason => "excluded and unavailable candidates must carry a reason",
            Self::UnexpectedReason => "only excluded and unavailable candidates carry a reason",
            Self::DetailWithoutReason => "detail narrows a reason and cannot stand alone",
        })
    }
}

impl std::error::Error for ParticipationError {}

impl Participation {
    /// A candidate that took part.
    pub fn active(digest: Digest) -> Self {
        Self {
            digest,
            state: ParticipationState::Active,
            reason: None,
            detail: None,
        }
    }

    /// A candidate that did not, with the reason it owes.
    pub fn excluded(digest: Digest, reason: ParticipationReason) -> Self {
        Self {
            digest,
            state: ParticipationState::Excluded,
            reason: Some(reason),
            detail: None,
        }
    }

    /// Attach human-facing narrowing text.
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Check the state/reason pairing.
    ///
    /// The invariant worth the code: a refusal with no reason is exactly the
    /// blank cell Explain/Compare cannot explain, and it is silent — the
    /// record still serializes, still renders, and still says nothing. The
    /// constructors above make the legal shapes easy; this catches what came
    /// off a wire or a hand-built row.
    pub fn validate(&self) -> Result<(), ParticipationError> {
        match (self.state.requires_reason(), self.reason.is_some()) {
            (true, false) => Err(ParticipationError::MissingReason),
            (false, true) => Err(ParticipationError::UnexpectedReason),
            _ if self.detail.is_some() && self.reason.is_none() => {
                Err(ParticipationError::DetailWithoutReason)
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest() -> Digest {
        Digest::from_hex("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
            .expect("valid")
    }

    #[test]
    fn exactly_the_refusing_states_owe_a_reason() {
        // ALL, not a re-typed list: the macro derives it from the enum's own
        // declaration, so a sixth state reaches this filter the day it exists.
        let owing: Vec<_> = ParticipationState::ALL
            .into_iter()
            .filter(|s| s.requires_reason())
            .collect();
        assert_eq!(
            owing,
            vec![
                ParticipationState::Excluded,
                ParticipationState::Unavailable
            ]
        );
    }

    #[test]
    fn an_unexplained_refusal_does_not_validate() {
        let silent = Participation {
            digest: digest(),
            state: ParticipationState::Excluded,
            reason: None,
            detail: None,
        };
        assert_eq!(silent.validate(), Err(ParticipationError::MissingReason));

        let explained = Participation::excluded(digest(), ParticipationReason::PinDrift);
        assert_eq!(explained.validate(), Ok(()));
    }

    #[test]
    fn a_reason_on_a_non_refusal_does_not_validate() {
        let confused = Participation {
            digest: digest(),
            state: ParticipationState::Active,
            reason: Some(ParticipationReason::BudgetCut),
            detail: None,
        };
        assert_eq!(
            confused.validate(),
            Err(ParticipationError::UnexpectedReason)
        );
    }

    #[test]
    fn detail_cannot_stand_without_a_reason() {
        let dangling = Participation::active(digest()).with_detail("because I said so");
        assert_eq!(
            dangling.validate(),
            Err(ParticipationError::DetailWithoutReason)
        );
        // But detail alongside a reason is the intended shape.
        assert_eq!(
            Participation::excluded(digest(), ParticipationReason::BudgetCut)
                .with_detail("over the 8k declared ceiling")
                .validate(),
            Ok(())
        );
    }

    #[test]
    fn unknown_fields_are_refused_rather_than_dropped() {
        let json = r#"{
            "digest": "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "state": "active",
            "invented_by_a_caller": true
        }"#;
        assert!(serde_json::from_str::<Participation>(json).is_err());
    }

    #[test]
    fn reasons_serialize_as_snake_case_tokens() {
        assert_eq!(
            serde_json::to_string(&ParticipationReason::PrecedenceLoss).expect("ok"),
            "\"precedence_loss\""
        );
        // These counts pin the enums against their externally documented sets
        // (the D5 exclusion vocabulary and the five-state participation model).
        // They used to be tautologies — `ALL.len()` on a hand-typed `[Self; N]`
        // is `N` by type — but `ALL` is now macro-derived from the enum's own
        // declaration, so a ninth reason or a sixth state moves the left side
        // and this is the test that reds. The DDL parity gate in
        // `xtask/src/contract.rs` holds the SQL CHECK token sets to the same
        // lists.
        assert_eq!(ParticipationReason::ALL.len(), 8);
        assert_eq!(ParticipationState::ALL.len(), 5);
    }
}
