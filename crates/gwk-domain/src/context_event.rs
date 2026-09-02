//! The Context lifecycle WRITE grammar: the ten facts, the one command
//! that records them, and the payload the log holds.
//!
//! These shapes live here rather than beside the read grammar in
//! `gwk-context` for a packaging reason that is mechanical, not stylistic.
//! `gwk-kernel` is published and `gwk-context` is not, and `cargo package`
//! refuses that dependency in both directions — declare a version on an
//! unpublished crate and packaging resolves it from the crates.io index
//! where it is absent; omit the version and packaging refuses it for having
//! none. The kernel is the process that deserializes these facts off the log
//! and projects them into the four truth tables, so it must be able to name
//! them. E8 hit the same bind and put the Context CAS port and class
//! vocabulary in [`crate::context`]; this module is the same answer for the
//! lifecycle grammar. `gwk-context` re-exports every name below, so the read
//! grammar and its callers still spell them the way they always did.
//!
//! Nothing here is live. Context is mandatory from protocol major 2, which
//! the kernel refuses; see `gwk_context::wire` for that staging and for the
//! read grammar these facts are eventually read back through.
//!
//! Not to be confused with [`crate::engine::LifecycleFact`], which is an
//! engine adapter's report about a spawned process. This is the Context
//! plane's own truth vocabulary; the two never meet.
//!
//! ## CTX-12 — attribution is provenance, not authorization
//!
//! Re-disclosed here because 8A is the phase required to state it, and because
//! this module is where it becomes structural rather than a promise.
//!
//! Same-EUID remains the entire authentication boundary for the kernel socket.
//! Context lifecycle events inherit that verbatim from threat 10; nothing in
//! this grammar narrows it and nothing here should be read as doing so. What
//! this grammar adds is one Context-scoped control: **a client cannot supply
//! source attribution.** [`RecordContextFact`] carries exactly one field and it
//! is the fact. [`ContextAttribution`] appears only on
//! [`ContextEventPayload`] — the kernel-side shape — and the compiler derives
//! it by re-reading its own resolved manifest rather than by trusting anything
//! a caller said about itself.
//!
//! The asymmetry is the mitigation, so it is asserted rather than described —
//! and asserted against the GENERATED surface rather than a list. The unit test
//! here walks a hand-written sample of each fact, which is a fast local check
//! and was, on its own, a guard with a hole the size of the next variant: an
//! eleventh fact carrying an `actor` and mapped onto an existing event name
//! passed it, because the sample list still held ten entries. The binding check
//! is `inspect_context_attribution` in `xtask`, which reads the generated
//! TypeScript — every variant and every field, by construction — and fails if
//! any client-submittable type declares an attribution-shaped property.
//!
//! ## Events ride the existing envelope
//!
//! There is no closed `KernelEvent` sum type in this system and this module
//! does not invent one. [`crate::EventEnvelope`] carries open bounded
//! `aggregate_type` / `event_type` strings over a generic JSON payload, so the
//! ten D4 lifecycle events are ten new `event_type` values under three new
//! `aggregate_type` values. [`ContextEventName`] and [`ContextAggregate`] are
//! closed enums over exactly those strings — closed on this side, open on the
//! envelope's, which is what lets the log accept them without a contract
//! change while every GridWork reader still branches exhaustively.

use crate::context_digest::Digest;
use crate::context_truth::{
    Assurance, EvidenceRefs, FinalizationSupplementId, ManifestId, ObservationIndex,
    ObservationSupplementId, ParticipationRecords, RecordCount, ReleaseSupplementId, context_id,
};
use crate::ids::{AttemptId, ByteCount, Timestamp};

/// Maximum UTF-8 bytes in a compiler-derived attribution component.
pub const CONTEXT_ATTRIBUTION_MAX_BYTES: usize = 128;
/// Maximum routes one optimization candidate may declare it affects.
pub const CONTEXT_CANDIDATE_ROUTE_MAX_COUNT: usize = 64;

/// Why a wire-v2 Context value failed validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextWireError {
    /// An attribution component was empty.
    EmptyAttribution,
    /// An attribution component exceeded its byte bound.
    AttributionTooLong,
    /// A candidate declared more affected routes than the bound allows.
    TooManyCandidateRoutes,
    /// A projection read asked for zero rows or more than the bound allows.
    QueryLimitOutOfRange,
    /// A graph read asked for zero hops or more than the bound allows.
    GraphDepthOutOfRange,
    /// A comparison named the same subject twice.
    CompareSubjectsIdentical,
    /// A comparison asked for no stages, or more than there are.
    CompareStagesOutOfRange,
    /// A comparison named the same stage more than once.
    CompareStagesRepeated,
}

impl std::fmt::Display for ContextWireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::EmptyAttribution => "attribution component must not be empty",
            Self::AttributionTooLong => "attribution component exceeds its byte bound",
            Self::TooManyCandidateRoutes => "candidate declares too many affected routes",
            Self::QueryLimitOutOfRange => "projection limit must be nonzero and inside its bound",
            Self::GraphDepthOutOfRange => "graph depth must be nonzero and inside its bound",
            Self::CompareSubjectsIdentical => "comparison requires two distinct subjects",
            Self::CompareStagesOutOfRange => {
                "comparison must name at least one stage and no more than there are"
            }
            Self::CompareStagesRepeated => "comparison named the same stage twice",
        })
    }
}

impl std::error::Error for ContextWireError {}

context_id!(
    /// One Context run: the span from opening a rendered manifest to closing it.
    ///
    /// Distinct from `AttemptId` on purpose. An attempt is one spawn; a run is
    /// the Context-plane lifetime around it, and ADR-0032 lists run and attempt
    /// as separate stable IDs precisely so a projection can join them rather
    /// than assume they are the same thing.
    ContextRunId
);
context_id!(
    /// One immutable optimization candidate.
    OptimizationCandidateId
);

/// A compiler-DERIVED attribution component. Never client-supplied (CTX-12).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct AttributionPart(String);

impl AttributionPart {
    pub fn parse(value: &str) -> Result<Self, ContextWireError> {
        if value.is_empty() {
            return Err(ContextWireError::EmptyAttribution);
        }
        if value.len() > CONTEXT_ATTRIBUTION_MAX_BYTES {
            return Err(ContextWireError::AttributionTooLong);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for AttributionPart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for AttributionPart {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        Self::parse(raw.as_ref()).map_err(serde::de::Error::custom)
    }
}

// Attribution components are validated strings on the wire.
impl specta::Type for AttributionPart {
    fn definition(types: &mut specta::Types) -> specta::datatype::DataType {
        <String as specta::Type>::definition(types)
    }
}

/// A bounded count of routes one optimization candidate declares it affects.
///
/// A newtype rather than a `RecordCount` plus a free checking function, because
/// the free function had no call site and `RecordCount` bounds at 65,535 — so a
/// candidate could declare sixty-five thousand affected routes past a constant
/// that said sixty-four. A bound the wire does not enforce is not a bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct RouteCount(u32);

impl RouteCount {
    pub fn new(value: u32) -> Result<Self, ContextWireError> {
        if value as usize > CONTEXT_CANDIDATE_ROUTE_MAX_COUNT {
            return Err(ContextWireError::TooManyCandidateRoutes);
        }
        Ok(Self(value))
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

impl<'de> serde::Deserialize<'de> for RouteCount {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(u32::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

// A validated count on the wire.
impl specta::Type for RouteCount {
    fn definition(types: &mut specta::Types) -> specta::datatype::DataType {
        <u32 as specta::Type>::definition(types)
    }
}

// Three free functions stood here — `check_candidate_routes`,
// `check_candidate_summary`, and `check_wire_id` — and all three were dead.
//
// They are gone rather than wired up, because each was a different way of
// advertising a bound that did not exist. `check_candidate_routes` had no call
// site, so `affected_route_count` was bounded only by `RecordCount`'s 65,535: a
// candidate could declare sixty-five thousand affected routes past a constant
// that said sixty-four. `check_candidate_summary` guarded a `summary` field the
// grammar does not have. And `check_wire_id` re-implemented the LENGTH half of
// `id_is_valid`, which every `context_id!` type already runs in full — a
// weaker second copy of a rule, added in the same change whose message argued
// that a second copy is how one rule becomes two.
//
// The route bound is now enforced where a bound has to be, on the way in: see
// `RouteCount`. The other two are deleted along with their constants and error
// variants. A constant nothing consults is not documentation; it reads as a
// promise the wire does not keep.

// ============================================================
// Aggregates and event names
// ============================================================

// Both closed sets below are generated from ONE list each, and that is the
// whole reason the macros exist.
//
// The previous shape declared the variants, then declared `ALL` as a second
// array literal, then wrote `as_str` and `aggregate` as third and fourth
// exhaustive matches. Adding a variant broke the matches — but `ALL` was just
// an array, and `ALL.len()` on a `[Self; 10]` is `10` BY TYPE. So
// `assert_eq!(ALL.len(), 10)` compiled to `assert_eq!(10, 10)`: it pinned the
// array's declared arity against a literal and could not observe the enum at
// all. An eleventh variant, added to every match and left out of `ALL`, passed
// every test in this file including the one whose comment claims to catch
// exactly that.
//
// A macro is more machinery than an enum deserves. It is here because one list
// is the only version of "these agree" that does not depend on four places
// being edited together, and the count assertion that was supposed to enforce
// that turned out to assert nothing.

macro_rules! context_aggregates {
    ($($(#[$doc:meta])* $variant:ident => $wire:literal),* $(,)?) => {
        /// The `aggregate_type` families Context writes into the one kernel log.
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
        pub enum ContextAggregate {
            $(
                $(#[$doc])*
                #[serde(rename = $wire)]
                $variant,
            )*
        }

        impl ContextAggregate {
            pub const ALL: [Self; [$(stringify!($variant)),*].len()] = [$(Self::$variant),*];

            /// The exact `EventEnvelope::aggregate_type` string.
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $wire,)* }
            }
        }
    };
}

context_aggregates! {
    /// Resolution and release of one immutable manifest.
    ContextManifest => "context_manifest",
    /// The observed lifetime around one rendered manifest.
    ContextRun => "context_run",
    /// Proposed and dispositioned optimization candidates.
    ContextOptimization => "context_optimization",
}

impl std::fmt::Display for ContextAggregate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

macro_rules! context_event_names {
    ($($(#[$doc:meta])* $variant:ident => $wire:literal @ $aggregate:ident),* $(,)?) => {
        /// The ten D4 lifecycle `event_type` values.
        ///
        /// Ten because ADR-0032 decision 4 names ten. Verification and rejection
        /// are ONE name carrying a verdict, which is how the ADR names them too —
        /// the alternative spends a name on a field and makes "was it verified?"
        /// a question about which of two event types arrived.
        ///
        /// The wire string is written once per variant and reaches both the
        /// serde tag and `as_str` from there. It used to be written twice, and
        /// `rename_all = "snake_case"` derived one of them from the VARIANT name
        /// while `as_str` spelled out the aggregate-prefixed one — the same value
        /// under two names, the envelope carrying one and the payload beside it
        /// carrying the other.
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
        pub enum ContextEventName {
            $(
                $(#[$doc])*
                #[serde(rename = $wire)]
                $variant,
            )*
        }

        impl ContextEventName {
            pub const ALL: [Self; [$(stringify!($variant)),*].len()] = [$(Self::$variant),*];

            /// The exact `EventEnvelope::event_type` string.
            ///
            /// Prefixed by its aggregate, matching `task` / `task_state_changed`
            /// in the existing log. The prefix is not decoration: `event_type` is
            /// globally open, so an unprefixed `run_closed` would collide with
            /// the first other aggregate that closes a run.
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $wire,)* }
            }

            /// The aggregate family this event belongs to.
            pub const fn aggregate(self) -> ContextAggregate {
                match self { $(Self::$variant => ContextAggregate::$aggregate,)* }
            }
        }
    };
}

context_event_names! {
    CompilationRequested => "context_manifest_compilation_requested" @ ContextManifest,
    ManifestResolved => "context_manifest_resolved" @ ContextManifest,
    ManifestVerificationRecorded => "context_manifest_verification_recorded" @ ContextManifest,
    ReleaseRecorded => "context_manifest_release_recorded" @ ContextManifest,
    RunOpened => "context_run_opened" @ ContextRun,
    ObservationAppended => "context_run_observation_appended" @ ContextRun,
    RunClosed => "context_run_closed" @ ContextRun,
    AssuranceCertified => "context_run_assurance_certified" @ ContextRun,
    OptimizationCandidateProposed => "context_optimization_candidate_proposed" @ ContextOptimization,
    OptimizationCandidateDispositioned
        => "context_optimization_candidate_dispositioned" @ ContextOptimization,
}

impl std::fmt::Display for ContextEventName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ============================================================
// Verdicts and dispositions
// ============================================================

/// The independent verifier's answer for one exact manifest digest.
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
pub enum VerificationVerdict {
    /// Every checked property held for this digest.
    Verified,
    /// At least one checked property failed. The verifier records which.
    Rejected,
}

/// What an authored review decided about an optimization candidate.
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
pub enum CandidateDisposition {
    /// Accepted for application through the authored review path.
    Applied,
    /// Declined. The candidate stays immutable and recorded.
    Declined,
    /// Superseded by a later candidate over the same subject.
    Superseded,
}

// ============================================================
// The ten lifecycle facts
// ============================================================

/// One Context lifecycle fact, in the ten shapes D4 names.
///
/// This single enum is the vocabulary for BOTH directions: it is what a client
/// asks the kernel to record ([`RecordContextFact`]) and what the log holds
/// ([`ContextEventPayload`]). One enum rather than two parallel ones, because
/// two would drift and the drift would be invisible — the wrapper types are
/// where the two directions legitimately differ, and the only difference is
/// attribution.
///
/// Every variant is a terminal fact. Nothing here transitions a state machine,
/// which is why the command wrapper is `Record*` and not `Transition*`.
///
/// # Four of the ten write a row, and each carries the whole of it
///
/// [`Self::ManifestResolved`], [`Self::ReleaseRecorded`],
/// [`Self::ObservationAppended`] and [`Self::AssuranceCertified`] project into
/// `gwk.context_manifest`, `_release`, `_observation` and `_finalization`
/// respectively. Each of those four carries every column of its row, including
/// values another fact already stated.
///
/// The alternative was a projection that joins four other event types back out
/// of the log, and it fails in two directions at once. Columns that no fact
/// carried at all — `tool_schema_count`, `observed_bytes`,
/// `visible_source_count`, `approval_count`, and `evidence_ids` on every one of
/// the four — would have to be written as zeros and empty arrays that pass
/// their CHECKs and record nothing true, permanently, on tables that admit no
/// UPDATE. And the joins themselves need selection rules nothing declares: two
/// `CompilationRequested` events for one attempt after a route change is
/// ordinary, and no rule says which one the manifest row means.
///
/// So the rule is: the fact that writes a row states the row. The remaining six
/// facts write nothing and are read as history — which is a claim the applier
/// makes structurally, by matching all ten with six empty arms.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(tag = "fact", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContextFact {
    /// A compilation was asked for, before any manifest exists.
    CompilationRequested {
        attempt_id: AttemptId,
        route_digest: Digest,
        authority_digest: Digest,
        requested_at: Timestamp,
    },
    /// The compiler emitted one immutable manifest.
    ///
    /// Carries `route_digest` and `authority_digest` even though
    /// [`Self::CompilationRequested`] already stated them for the same attempt.
    /// The duplication is deliberate: `gwk.context_manifest` declares both
    /// `NOT NULL`, and sourcing them from a prior event would make the row
    /// depend on a second fact that a partial log may not hold.
    ManifestResolved {
        manifest_id: ManifestId,
        attempt_id: AttemptId,
        manifest_digest: Digest,
        route_digest: Digest,
        authority_digest: Digest,
        source_count: RecordCount,
        source_bytes: ByteCount,
        participations: ParticipationRecords,
        evidence_ids: EvidenceRefs,
        resolved_at: Timestamp,
    },
    /// The independent verifier answered for one exact digest.
    ManifestVerificationRecorded {
        manifest_id: ManifestId,
        manifest_digest: Digest,
        verdict: VerificationVerdict,
        verification_digest: Digest,
        evidence_ids: EvidenceRefs,
        verified_at: Timestamp,
    },
    /// Exactly what was rendered and released to an engine.
    ReleaseRecorded {
        manifest_id: ManifestId,
        release_id: ReleaseSupplementId,
        rendered_digest: Digest,
        tool_schema_digest: Digest,
        rendered_bytes: ByteCount,
        tool_schema_count: RecordCount,
        evidence_ids: EvidenceRefs,
        released_at: Timestamp,
    },
    /// A Context run opened around a released manifest.
    RunOpened {
        run_id: ContextRunId,
        manifest_id: ManifestId,
        release_id: ReleaseSupplementId,
        opened_at: Timestamp,
    },
    /// One ordered post-boundary observation was appended.
    ///
    /// `visible_source_count` is the visibility limit `truncated` is a flag
    /// about, so the two travel together: a row saying `truncated` with nothing
    /// beside it records that something was cut and refuses to say from what.
    ObservationAppended {
        run_id: ContextRunId,
        manifest_id: ManifestId,
        observation_id: ObservationSupplementId,
        observation_index: ObservationIndex,
        fact_digest: Digest,
        observed_bytes: ByteCount,
        visible_source_count: RecordCount,
        truncated: bool,
        evidence_ids: EvidenceRefs,
        observed_at: Timestamp,
    },
    /// The run closed. Assurance is a separate later fact.
    ///
    /// Writes no row. `gwk.context_finalization` is written once, by
    /// [`Self::AssuranceCertified`], which restates what it certifies; a run
    /// that closes and is never certified therefore has no finalization row.
    /// That is the honest reading of an append-only table, and it means
    /// `lifecycle_complete` is only ever persisted for runs that reached
    /// certification.
    RunClosed {
        run_id: ContextRunId,
        finalization_id: FinalizationSupplementId,
        output_digest: Digest,
        observation_count: RecordCount,
        lifecycle_complete: bool,
        closed_at: Timestamp,
    },
    /// A closed run was certified at an assurance level.
    ///
    /// This is the sole writer of `gwk.context_finalization`, so it carries the
    /// whole row. That is forced, not chosen: `final_event_root` and `assurance`
    /// exist only here, `output_digest` carries a `sha256:` CHECK so a
    /// close-time insert would have to fabricate one, and the table admits no
    /// UPDATE at three layers — an `ENABLE ALWAYS` BEFORE UPDATE OR DELETE
    /// trigger, `GrantClass::History`, and an explicit REVOKE. A row cannot be
    /// opened at close and completed at certification.
    ///
    /// `closed_at` is restated from [`Self::RunClosed`] because
    /// `finalized_at` takes the close time rather than the attestation time:
    /// every other field of the row that means a time-of-fact is a run-close
    /// field, and `certified_at` dates a later statement about an already
    /// settled run. The two are not the same instant and the column can hold
    /// only one.
    AssuranceCertified {
        run_id: ContextRunId,
        manifest_id: ManifestId,
        finalization_id: FinalizationSupplementId,
        output_digest: Digest,
        verification_digest: Digest,
        approval_count: RecordCount,
        observation_count: RecordCount,
        final_event_root: Digest,
        lifecycle_complete: bool,
        assurance: Assurance,
        evidence_ids: EvidenceRefs,
        closed_at: Timestamp,
        certified_at: Timestamp,
    },
    /// Optimization proposed an immutable candidate. It writes no truth.
    OptimizationCandidateProposed {
        candidate_id: OptimizationCandidateId,
        patch_digest: Digest,
        expected_effect_digest: Digest,
        affected_route_count: RouteCount,
        evidence_ids: EvidenceRefs,
        proposed_at: Timestamp,
    },
    /// An authored review dispositioned a candidate.
    OptimizationCandidateDispositioned {
        candidate_id: OptimizationCandidateId,
        disposition: CandidateDisposition,
        review_digest: Digest,
        dispositioned_at: Timestamp,
    },
}

impl ContextFact {
    /// The event this fact becomes in the log.
    ///
    /// An exhaustive match rather than a lookup table: a new variant fails to
    /// compile here, which is the only version of "the mapping is complete"
    /// that survives someone adding an eleventh fact in a hurry.
    pub const fn event_name(&self) -> ContextEventName {
        match self {
            Self::CompilationRequested { .. } => ContextEventName::CompilationRequested,
            Self::ManifestResolved { .. } => ContextEventName::ManifestResolved,
            Self::ManifestVerificationRecorded { .. } => {
                ContextEventName::ManifestVerificationRecorded
            }
            Self::ReleaseRecorded { .. } => ContextEventName::ReleaseRecorded,
            Self::RunOpened { .. } => ContextEventName::RunOpened,
            Self::ObservationAppended { .. } => ContextEventName::ObservationAppended,
            Self::RunClosed { .. } => ContextEventName::RunClosed,
            Self::AssuranceCertified { .. } => ContextEventName::AssuranceCertified,
            Self::OptimizationCandidateProposed { .. } => {
                ContextEventName::OptimizationCandidateProposed
            }
            Self::OptimizationCandidateDispositioned { .. } => {
                ContextEventName::OptimizationCandidateDispositioned
            }
        }
    }
}

// ============================================================
// Commands — what a client may submit
// ============================================================

/// The v2 Context write grammar, in full.
///
/// One shape, one field. Recording a fact is the ONLY Context write, because
/// Context may compile, verify, attest, project, explain, compare and suggest —
/// and may not independently authorize work or write execution truth outside
/// kernel commands (ADR-0032). A grammar with a second verb would be a second
/// write authority wearing a smaller name.
///
/// **There is no actor field here and there must never be one.** See CTX-12 in
/// the module docs; the omission is asserted by test, not left to review.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct RecordContextFact {
    pub fact: ContextFact,
}

// ============================================================
// Events — what the log holds
// ============================================================

/// Source attribution for one Context lifecycle event.
///
/// Compiler-DERIVED. Every field is re-read from the resolved manifest the
/// compiler itself produced; none is copied from a request. That is the whole
/// of the CTX-12 control, and it is why this type appears on the event payload
/// and nowhere in [`RecordContextFact`].
///
/// This is provenance, not authorization. It answers "which compiler run
/// produced this, against which route and authority" — it never answers "was
/// this permitted", which stays same-EUID at the socket.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct ContextAttribution {
    /// The compiler build that resolved the manifest.
    pub compiler: AttributionPart,
    /// The route the manifest was resolved against.
    pub route_digest: Digest,
    /// The authority set in force at resolution.
    pub authority_digest: Digest,
    /// The manifest the compiler re-read to derive the fields above.
    pub derived_from: ManifestId,
}

/// One Context lifecycle event's payload, as it lands in the kernel log.
///
/// Rides [`crate::EventEnvelope`]'s generic `payload`; the envelope's own
/// `aggregate_type` and `event_type` carry [`ContextEventName::aggregate`] and
/// [`ContextEventName::as_str`]. `name` is repeated inside the payload on
/// purpose: the envelope's copy is an open string a non-GridWork reader may
/// have written, and a projection that branches on the payload should branch on
/// the closed value it can exhaust.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct ContextEventPayload {
    pub name: ContextEventName,
    pub attribution: ContextAttribution,
    pub fact: ContextFact,
}

#[cfg(test)]
mod tests {
    use crate::ids::EvidenceId;

    use super::*;

    fn count(value: u32) -> RecordCount {
        RecordCount::new(value).expect("bounded count")
    }

    fn evidence() -> EvidenceRefs {
        EvidenceRefs::new(vec![EvidenceId::new("evidence-1")]).expect("bounded evidence")
    }

    fn digest() -> Digest {
        Digest::parse(&format!("sha256:{}", "a".repeat(64))).expect("valid digest")
    }

    fn manifest_id() -> ManifestId {
        ManifestId::parse("manifest-1").expect("valid id")
    }

    fn run_id() -> ContextRunId {
        ContextRunId::parse("run-1").expect("valid id")
    }

    fn timestamp() -> Timestamp {
        Timestamp::new("2026-08-14T00:00:00Z")
    }

    fn every_fact() -> Vec<ContextFact> {
        vec![
            ContextFact::CompilationRequested {
                attempt_id: AttemptId::new("attempt-1"),
                route_digest: digest(),
                authority_digest: digest(),
                requested_at: timestamp(),
            },
            ContextFact::ManifestResolved {
                manifest_id: manifest_id(),
                attempt_id: AttemptId::new("attempt-1"),
                manifest_digest: digest(),
                route_digest: digest(),
                authority_digest: digest(),
                source_count: count(1),
                source_bytes: ByteCount::new(1),
                participations: ParticipationRecords::new(Vec::new()).expect("empty is valid"),
                evidence_ids: evidence(),
                resolved_at: timestamp(),
            },
            ContextFact::ManifestVerificationRecorded {
                manifest_id: manifest_id(),
                manifest_digest: digest(),
                verdict: VerificationVerdict::Verified,
                verification_digest: digest(),
                evidence_ids: EvidenceRefs::new(Vec::new()).expect("empty is valid"),
                verified_at: timestamp(),
            },
            ContextFact::ReleaseRecorded {
                manifest_id: manifest_id(),
                release_id: ReleaseSupplementId::parse("release-1").expect("valid id"),
                rendered_digest: digest(),
                tool_schema_digest: digest(),
                rendered_bytes: ByteCount::new(1),
                tool_schema_count: count(1),
                evidence_ids: evidence(),
                released_at: timestamp(),
            },
            ContextFact::RunOpened {
                run_id: run_id(),
                manifest_id: manifest_id(),
                release_id: ReleaseSupplementId::parse("release-1").expect("valid id"),
                opened_at: timestamp(),
            },
            ContextFact::ObservationAppended {
                run_id: run_id(),
                manifest_id: manifest_id(),
                observation_id: ObservationSupplementId::parse("observation-1").expect("valid id"),
                observation_index: ObservationIndex::new(1).expect("valid index"),
                fact_digest: digest(),
                observed_bytes: ByteCount::new(1),
                visible_source_count: count(1),
                truncated: false,
                evidence_ids: evidence(),
                observed_at: timestamp(),
            },
            ContextFact::RunClosed {
                run_id: run_id(),
                finalization_id: FinalizationSupplementId::parse("final-1").expect("valid id"),
                output_digest: digest(),
                observation_count: count(1),
                lifecycle_complete: true,
                closed_at: timestamp(),
            },
            ContextFact::AssuranceCertified {
                run_id: run_id(),
                manifest_id: manifest_id(),
                finalization_id: FinalizationSupplementId::parse("final-1").expect("valid id"),
                output_digest: digest(),
                verification_digest: digest(),
                approval_count: count(0),
                observation_count: count(1),
                final_event_root: digest(),
                lifecycle_complete: true,
                assurance: Assurance::Trace,
                evidence_ids: evidence(),
                closed_at: timestamp(),
                certified_at: timestamp(),
            },
            ContextFact::OptimizationCandidateProposed {
                candidate_id: OptimizationCandidateId::parse("candidate-1").expect("valid id"),
                patch_digest: digest(),
                expected_effect_digest: digest(),
                affected_route_count: RouteCount::new(1).expect("valid count"),
                evidence_ids: EvidenceRefs::new(Vec::new()).expect("empty is valid"),
                proposed_at: timestamp(),
            },
            ContextFact::OptimizationCandidateDispositioned {
                candidate_id: OptimizationCandidateId::parse("candidate-1").expect("valid id"),
                disposition: CandidateDisposition::Applied,
                review_digest: digest(),
                dispositioned_at: timestamp(),
            },
        ]
    }

    #[test]
    fn there_are_exactly_ten_lifecycle_events_and_every_one_has_a_fact() {
        // The count first. Everything below folds over a collection, and a fold
        // over a short collection agrees with itself just as happily.
        assert_eq!(ContextEventName::ALL.len(), 10, "ADR-0032 D4 names ten");

        let facts = every_fact();
        assert_eq!(facts.len(), 10, "one fact per lifecycle event");

        // Bidirectional: every name is produced by some fact, and every fact
        // produces a name in ALL. Either direction alone passes a mapping that
        // sends two facts to one name.
        let produced: Vec<ContextEventName> = facts.iter().map(ContextFact::event_name).collect();
        for name in ContextEventName::ALL {
            assert!(produced.contains(&name), "no fact produces {name}");
        }
        for name in &produced {
            assert!(ContextEventName::ALL.contains(name), "{name} is not in ALL");
        }
    }

    #[test]
    fn every_event_name_is_prefixed_by_its_own_aggregate() {
        // The prefix is what keeps an open `event_type` namespace collision-free.
        // Asserting it here is cheaper than discovering a collision in the log.
        assert_eq!(ContextEventName::ALL.len(), 10);
        for name in ContextEventName::ALL {
            let aggregate = name.aggregate();
            assert!(
                name.as_str().starts_with(aggregate.as_str()),
                "{} is not prefixed by {}",
                name.as_str(),
                aggregate.as_str()
            );
        }
        // And every aggregate is actually used by something.
        assert_eq!(ContextAggregate::ALL.len(), 3);
        for aggregate in ContextAggregate::ALL {
            assert!(
                ContextEventName::ALL
                    .iter()
                    .any(|n| n.aggregate() == aggregate),
                "{aggregate} owns no event"
            );
        }
    }

    #[test]
    fn no_context_command_can_carry_attribution() {
        // CTX-12, mechanically. A client must not be able to assert who it is
        // on a fact that becomes provenance. This walks the SERIALIZED form
        // rather than the type, because a field added with a serde rename is
        // still a field on the wire.
        let facts = every_fact();
        assert_eq!(facts.len(), 10, "the sweep must cover every fact");

        for fact in facts {
            let command = RecordContextFact { fact };
            let json = serde_json::to_value(&command).expect("serializes");
            let text = json.to_string();
            for forbidden in ["actor", "attribution", "compiler", "identity", "principal"] {
                assert!(
                    !text.contains(forbidden),
                    "{forbidden} appears in a client-submittable command: {text}"
                );
            }
        }
    }

    #[test]
    fn the_event_payload_is_the_command_plus_derived_attribution() {
        // The asymmetry the module doc promises, in one comparison: the same
        // fact, once as a client sends it and once as the log holds it.
        let fact = ContextFact::RunOpened {
            run_id: run_id(),
            manifest_id: manifest_id(),
            release_id: ReleaseSupplementId::parse("release-1").expect("valid id"),
            opened_at: timestamp(),
        };
        let command = serde_json::to_value(RecordContextFact { fact: fact.clone() }).expect("ok");
        let event = serde_json::to_value(ContextEventPayload {
            name: fact.event_name(),
            attribution: ContextAttribution {
                compiler: AttributionPart::parse("gwk-context/0.0.1").expect("valid"),
                route_digest: digest(),
                authority_digest: digest(),
                derived_from: manifest_id(),
            },
            fact,
        })
        .expect("ok");

        assert!(command.get("attribution").is_none());
        assert!(event.get("attribution").is_some());
        assert_eq!(
            command.get("fact"),
            event.get("fact"),
            "the fact itself must be byte-identical in both directions"
        );
    }

    #[test]
    fn the_route_bound_is_enforced_on_the_wire_and_not_only_in_a_helper() {
        // The previous version of this test called a free function that had no
        // call site anywhere. It proved the function and nothing about the
        // grammar: `affected_route_count` was a `RecordCount`, so a candidate
        // declaring 65,535 affected routes deserialized clean past a constant
        // that said 64. The bound now lives on the way in, which is the only
        // place a wire bound is a bound.
        assert_eq!(
            RouteCount::new(CONTEXT_CANDIDATE_ROUTE_MAX_COUNT as u32).map(RouteCount::get),
            Ok(CONTEXT_CANDIDATE_ROUTE_MAX_COUNT as u32)
        );
        assert_eq!(
            RouteCount::new(CONTEXT_CANDIDATE_ROUTE_MAX_COUNT as u32 + 1),
            Err(ContextWireError::TooManyCandidateRoutes)
        );

        // And through the deserializer, which is the path an attacker uses.
        let over = CONTEXT_CANDIDATE_ROUTE_MAX_COUNT + 1;
        let fact = format!(
            r#"{{"fact":"optimization_candidate_proposed","candidate_id":"c","patch_digest":"sha256:{h}","expected_effect_digest":"sha256:{h}","affected_route_count":{over},"evidence_ids":[],"proposed_at":"2026-08-14T00:00:00Z"}}"#,
            h = "a".repeat(64)
        );
        assert!(
            serde_json::from_str::<ContextFact>(&fact).is_err(),
            "a candidate past the route bound deserialized"
        );
        // The positive control: the same document at the bound must parse, or
        // the assertion above passes because the whole shape is unparseable.
        let at_edge = fact.replace(
            &format!(r#""affected_route_count":{over}"#),
            &format!(
                r#""affected_route_count":{}"#,
                CONTEXT_CANDIDATE_ROUTE_MAX_COUNT
            ),
        );
        serde_json::from_str::<ContextFact>(&at_edge).expect("the exact bound must parse");
    }

    #[test]
    fn serde_and_as_str_agree_on_one_spelling() {
        // Two spellings of one value is how a log grows a name nobody can
        // query: the envelope's `event_type` comes from `as_str`, the payload's
        // own copy comes from serde, and a reader filtering on one silently
        // misses rows written by the other.
        //
        // This asserts SERDE against `as_str`. It previously asserted `Display`
        // against `as_str` — but Display is implemented AS `as_str`, so it
        // compared a function to itself and passed while the two real spellings
        // disagreed on all ten names. A tautological assertion is worse than no
        // assertion; it occupies the place where the real one would go.
        assert_eq!(ContextEventName::ALL.len(), 10);
        for name in ContextEventName::ALL {
            let encoded = serde_json::to_value(name).expect("serializes");
            assert_eq!(
                encoded.as_str().expect("a string on the wire"),
                name.as_str(),
                "{name:?} serializes under a different name than it reports"
            );
            // And back, so the pair is a round trip rather than two encoders
            // that happen to agree in one direction.
            assert_eq!(
                serde_json::from_value::<ContextEventName>(encoded).expect("decodes"),
                name
            );
        }

        assert_eq!(ContextAggregate::ALL.len(), 3);
        for aggregate in ContextAggregate::ALL {
            let encoded = serde_json::to_value(aggregate).expect("serializes");
            assert_eq!(
                encoded.as_str().expect("a string on the wire"),
                aggregate.as_str(),
                "{aggregate:?} serializes under a different name than it reports"
            );
            assert_eq!(
                serde_json::from_value::<ContextAggregate>(encoded).expect("decodes"),
                aggregate
            );
        }
    }
    #[test]
    fn the_attribution_bound_is_enforced_at_construction_and_on_the_way_in() {
        // Split out of `bounded_scalars_refuse_zero_and_overflow` when
        // `AttributionPart` moved down here; the query-side scalars it used to
        // sit beside stayed with the read grammar. Both halves of the bound are
        // asserted, because the newtype hand-writes `Deserialize` precisely so
        // an out-of-range value arriving from the wire is refused too.
        assert_eq!(
            AttributionPart::parse(""),
            Err(ContextWireError::EmptyAttribution)
        );
        assert_eq!(
            AttributionPart::parse(&"a".repeat(CONTEXT_ATTRIBUTION_MAX_BYTES + 1)),
            Err(ContextWireError::AttributionTooLong)
        );
        assert!(serde_json::from_str::<AttributionPart>("\"\"").is_err());
        // The positive control: a legal component must still parse, or the
        // assertions above pass on a type nothing can construct.
        assert!(AttributionPart::parse("gwk-context/0.0.1").is_ok());
    }
}
