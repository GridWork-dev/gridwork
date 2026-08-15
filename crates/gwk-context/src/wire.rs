//! The wire-v2 Context grammar: commands, facts, events, reads, and the one
//! thing the handshake does NOT carry.
//!
//! **Nothing here is live.** [`gwk_domain::ProtocolVersion::V2`] is *known* to
//! the version grammar and *refused* by the kernel, and every type in this
//! module is a published shape rather than an accepted request. That staging is
//! the point: a grammar reviewed and frozen before anything speaks it is a
//! grammar whose first speaker cannot quietly renegotiate it.
//!
//! ## The two axes, and the one that does not move
//!
//! `ProtocolVersion` is the WIRE grammar. `gwk_domain::CONTRACT_VERSION` is the
//! DOMAIN contract — entity, event, and command shapes. They are separate axes
//! and this change moves exactly one of them (ruling R10 / fork F9). Adding a
//! known major is not an entity/event/command shape change, so
//! `CONTRACT_VERSION` stays `1` through the eventual cutover and bumps on its
//! own merits when a shape actually changes. A reflexive lockstep bump would
//! teach every future reader that the two always move together, which is the
//! belief R10 exists to prevent.
//!
//! ## Nothing Context-specific rides the hello
//!
//! Ruling R9 / fork F8. The handshake shape stays major/minor/capabilities/
//! client, and only the major flips. There is deliberately no `context_capable`
//! field and no `context` capability name, because ADR-0032 treats *mandatory*
//! and *capability* as opposites: a v2 major makes Context mandatory, full
//! stop. There is no optional v1 Context mode, no translator, no proxy, and no
//! dual stack — a peer either speaks a major that includes Context or it does
//! not connect. See [`CONTEXT_MANDATORY_FROM`].
//!
//! Hello is also per-CONNECTION while Context resolution is per-ATTEMPT, so a
//! connection-scoped flag would be answering a question at the wrong lifetime
//! even if the grammar wanted one.
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
//! does not invent one. [`gwk_domain::EventEnvelope`] carries open bounded
//! `aggregate_type` / `event_type` strings over a generic JSON payload, so the
//! ten D4 lifecycle events are ten new `event_type` values under three new
//! `aggregate_type` values. [`ContextEventName`] and [`ContextAggregate`] are
//! closed enums over exactly those strings — closed on this side, open on the
//! envelope's, which is what lets the log accept them without a contract
//! change while every GridWork reader still branches exhaustively.

use gwk_domain::{AttemptId, ByteCount, ProtocolVersion, Timestamp};

use crate::manifest::context_id;
use crate::{
    Assurance, ContextStage, Digest, EvidenceRefs, FinalizationSupplementId, ManifestId,
    ObservationIndex, ObservationSupplementId, ParticipationRecords, RecordCount,
    ReleaseSupplementId,
};

// ============================================================
// Handshake
// ============================================================

/// The major from which Context is MANDATORY.
///
/// Not "the major that supports Context" — there is no supported/unsupported
/// axis here. Below this major Context does not exist on the wire at all; at
/// and above it, every participant speaks it. That is the whole negotiation,
/// and it is why the hello has no Context field to negotiate with (R9).
pub const CONTEXT_MANDATORY_FROM: ProtocolVersion = ProtocolVersion::V2;

// ============================================================
// Bounds
// ============================================================

/// Maximum UTF-8 bytes in a compiler-derived attribution component.
pub const CONTEXT_ATTRIBUTION_MAX_BYTES: usize = 128;
/// Maximum routes one optimization candidate may declare it affects.
pub const CONTEXT_CANDIDATE_ROUTE_MAX_COUNT: usize = 64;
/// Maximum rows one projection read may ask for.
pub const CONTEXT_QUERY_LIMIT_MAX: u32 = 1_000;
/// Maximum edge hops a graph read may traverse.
pub const CONTEXT_GRAPH_DEPTH_MAX: u32 = 16;
/// Maximum stages one comparison may ask for.
///
/// `ContextStage::ALL` has five members, so anything past five is redundant by
/// construction and zero asks a comparison to compare nothing. Without this the
/// field was bounded only by the 4 MiB frame, in a module whose header claims
/// every count on this wire is bounded.
pub const CONTEXT_COMPARE_STAGE_MAX_COUNT: usize = 5;

// ============================================================
// Errors
// ============================================================

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

// ============================================================
// Identifiers
// ============================================================

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

// ============================================================
// Bounded scalars
// ============================================================

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

/// A bounded row limit for a projection read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct QueryLimit(u32);

impl QueryLimit {
    pub fn new(value: u32) -> Result<Self, ContextWireError> {
        if value == 0 || value > CONTEXT_QUERY_LIMIT_MAX {
            return Err(ContextWireError::QueryLimitOutOfRange);
        }
        Ok(Self(value))
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

impl<'de> serde::Deserialize<'de> for QueryLimit {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(u32::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

// A validated count on the wire.
impl specta::Type for QueryLimit {
    fn definition(types: &mut specta::Types) -> specta::datatype::DataType {
        <u32 as specta::Type>::definition(types)
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

/// A bounded hop count for a graph read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct GraphDepth(u32);

impl GraphDepth {
    pub fn new(value: u32) -> Result<Self, ContextWireError> {
        if value == 0 || value > CONTEXT_GRAPH_DEPTH_MAX {
            return Err(ContextWireError::GraphDepthOutOfRange);
        }
        Ok(Self(value))
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

impl<'de> serde::Deserialize<'de> for GraphDepth {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(u32::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

// A validated hop count on the wire.
impl specta::Type for GraphDepth {
    fn definition(types: &mut specta::Types) -> specta::datatype::DataType {
        <u32 as specta::Type>::definition(types)
    }
}

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
    ManifestResolved {
        manifest_id: ManifestId,
        attempt_id: AttemptId,
        manifest_digest: Digest,
        source_count: RecordCount,
        source_bytes: ByteCount,
        participations: ParticipationRecords,
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
    ObservationAppended {
        run_id: ContextRunId,
        observation_id: ObservationSupplementId,
        observation_index: ObservationIndex,
        fact_digest: Digest,
        truncated: bool,
        observed_at: Timestamp,
    },
    /// The run closed. Assurance is a separate later fact.
    RunClosed {
        run_id: ContextRunId,
        finalization_id: FinalizationSupplementId,
        output_digest: Digest,
        observation_count: RecordCount,
        lifecycle_complete: bool,
        closed_at: Timestamp,
    },
    /// A closed run was certified at an assurance level.
    AssuranceCertified {
        run_id: ContextRunId,
        finalization_id: FinalizationSupplementId,
        assurance: Assurance,
        final_event_root: Digest,
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
/// Rides [`gwk_domain::EventEnvelope`]'s generic `payload`; the envelope's own
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

// ============================================================
// Reads
// ============================================================

/// Which supplement of a manifest a read wants.
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
pub enum SupplementKind {
    Release,
    Observation,
    Finalization,
}

/// How a read names the manifest it wants.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(tag = "by", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManifestSelector {
    Id { manifest_id: ManifestId },
    Attempt { attempt_id: AttemptId },
}

/// What an Explain read is asking about.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(tag = "subject", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExplainSubject {
    /// Why this source did or did not participate.
    Source { digest: Digest },
    /// Why the precedence resolution landed where it did.
    Precedence,
    /// Why the manifest carries the participation set it carries.
    Participation,
}

/// One side of a comparison.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(tag = "of", rename_all = "snake_case", deny_unknown_fields)]
pub enum CompareSubject {
    Manifest { manifest_id: ManifestId },
    Run { run_id: ContextRunId },
}

/// The v2 Context read grammar.
///
/// Eight reads, matching ADR-0032's three projections plus the four record
/// reads and Compare. These are published shapes, not live handlers: 8A
/// publishes the grammar, and the projections behind it are later work.
///
/// Explain and Compare are powered by immutable projections linked to source
/// commits and CAS objects. Neither recomputes a historical manifest from
/// current files — a recomputed manifest answers a question about today and is
/// presented as an answer about the past, which is worse than no answer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(tag = "query", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContextQuery {
    /// One resolved manifest.
    Manifest { select: ManifestSelector },
    /// Supplements of one manifest, of one kind.
    Supplement {
        manifest_id: ManifestId,
        kind: SupplementKind,
        limit: QueryLimit,
    },
    /// One source as the manifest saw it, by content digest.
    Source {
        manifest_id: ManifestId,
        digest: Digest,
    },
    /// Sources, revisions, manifests, releases, observations, verifier
    /// receipts, and candidates — the provenance projection.
    ProvenanceGraph { root: ManifestId, depth: GraphDepth },
    /// Skills, memories, notes, concepts, symbols, files, and their citation
    /// and semantic relationships.
    SemanticGraph { root: Digest, depth: GraphDepth },
    /// Tasks, attempts, sessions, messages, tools, evidence, interventions,
    /// outcomes, and costs — execution causality, kept separate from the two
    /// above so nothing flattens into one ambiguous graph.
    ExecutionDag {
        run_id: ContextRunId,
        depth: GraphDepth,
    },
    /// Why sources participated, or did not.
    Explain {
        manifest_id: ManifestId,
        subject: ExplainSubject,
    },
    /// Declared, resolved, released, observed and final states across two
    /// subjects, at the stages asked for.
    Compare {
        left: CompareSubject,
        right: CompareSubject,
        stages: Vec<ContextStage>,
    },
}

impl ContextQuery {
    /// Reject a comparison of something with itself.
    ///
    /// Not a deserialization-time check: both sides are individually legal, and
    /// the rule is about their relationship. A self-comparison is always an
    /// empty diff, so answering it costs a projection read to learn nothing.
    pub fn validate(&self) -> Result<(), ContextWireError> {
        let Self::Compare {
            left,
            right,
            stages,
        } = self
        else {
            return Ok(());
        };
        if left == right {
            return Err(ContextWireError::CompareSubjectsIdentical);
        }
        if stages.is_empty() || stages.len() > CONTEXT_COMPARE_STAGE_MAX_COUNT {
            return Err(ContextWireError::CompareStagesOutOfRange);
        }
        // Duplicates are their own refusal rather than folded into the bound.
        // Five copies of one stage is inside the count and still asks the same
        // question five times, which a projection would answer five times.
        let mut seen = Vec::with_capacity(stages.len());
        for stage in stages {
            if seen.contains(&stage) {
                return Err(ContextWireError::CompareStagesRepeated);
            }
            seen.push(stage);
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

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
                source_count: RecordCount::new(1).expect("valid count"),
                source_bytes: ByteCount::new(1),
                participations: ParticipationRecords::new(Vec::new()).expect("empty is valid"),
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
                observation_id: ObservationSupplementId::parse("observation-1").expect("valid id"),
                observation_index: ObservationIndex::new(1).expect("valid index"),
                fact_digest: digest(),
                truncated: false,
                observed_at: timestamp(),
            },
            ContextFact::RunClosed {
                run_id: run_id(),
                finalization_id: FinalizationSupplementId::parse("final-1").expect("valid id"),
                output_digest: digest(),
                observation_count: RecordCount::new(1).expect("valid count"),
                lifecycle_complete: true,
                closed_at: timestamp(),
            },
            ContextFact::AssuranceCertified {
                run_id: run_id(),
                finalization_id: FinalizationSupplementId::parse("final-1").expect("valid id"),
                assurance: Assurance::Trace,
                final_event_root: digest(),
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
    fn bounded_scalars_refuse_zero_and_overflow() {
        assert_eq!(
            QueryLimit::new(0),
            Err(ContextWireError::QueryLimitOutOfRange)
        );
        assert_eq!(
            QueryLimit::new(CONTEXT_QUERY_LIMIT_MAX + 1),
            Err(ContextWireError::QueryLimitOutOfRange)
        );
        assert_eq!(
            QueryLimit::new(CONTEXT_QUERY_LIMIT_MAX).map(QueryLimit::get),
            Ok(CONTEXT_QUERY_LIMIT_MAX)
        );

        assert_eq!(
            GraphDepth::new(0),
            Err(ContextWireError::GraphDepthOutOfRange)
        );
        assert_eq!(
            GraphDepth::new(CONTEXT_GRAPH_DEPTH_MAX + 1),
            Err(ContextWireError::GraphDepthOutOfRange)
        );

        assert_eq!(
            AttributionPart::parse(""),
            Err(ContextWireError::EmptyAttribution)
        );
        assert_eq!(
            AttributionPart::parse(&"a".repeat(CONTEXT_ATTRIBUTION_MAX_BYTES + 1)),
            Err(ContextWireError::AttributionTooLong)
        );
    }

    #[test]
    fn a_bound_is_enforced_on_the_way_in_not_just_at_construction() {
        // The newtypes derive Serialize but hand-write Deserialize precisely so
        // an out-of-range value arriving from the wire is refused. A derived
        // Deserialize would accept it and this test is what notices.
        assert!(serde_json::from_str::<QueryLimit>("0").is_err());
        assert!(
            serde_json::from_str::<QueryLimit>(&(CONTEXT_QUERY_LIMIT_MAX + 1).to_string()).is_err()
        );
        assert!(serde_json::from_str::<GraphDepth>("0").is_err());
        assert!(serde_json::from_str::<AttributionPart>("\"\"").is_err());
    }

    #[test]
    fn unknown_fields_are_refused_across_the_grammar() {
        // deny_unknown_fields on every wire shape: an unknown field is either
        // version skew or a caller inventing contract, and both should be loud.
        assert!(
            serde_json::from_str::<RecordContextFact>(
                r#"{"fact":{"fact":"run_opened","run_id":"r","manifest_id":"m","release_id":"x","opened_at":"2026-08-14T00:00:00Z"},"actor":"me"}"#
            )
            .is_err(),
            "an actor field must not deserialize onto a command"
        );
        assert!(
            serde_json::from_str::<ContextQuery>(
                r#"{"query":"manifest","select":{"by":"id","manifest_id":"m"},"limit":5}"#
            )
            .is_err(),
            "an unknown field must not deserialize onto a query"
        );

        // The positive controls. Both assertions above are `is_err`, so if
        // either shape stopped deserializing for an unrelated reason the test
        // would pass having proved nothing.
        serde_json::from_str::<RecordContextFact>(
            r#"{"fact":{"fact":"run_opened","run_id":"r","manifest_id":"m","release_id":"x","opened_at":"2026-08-14T00:00:00Z"}}"#,
        )
        .expect("the same command without the stray field must parse");
        serde_json::from_str::<ContextQuery>(
            r#"{"query":"manifest","select":{"by":"id","manifest_id":"m"}}"#,
        )
        .expect("the same query without the stray field must parse");

        // And one level down: a stray field inside the selector, not just
        // beside it. A guard that only reads the outermost object is one
        // nesting level from useless.
        assert!(
            serde_json::from_str::<ContextQuery>(
                r#"{"query":"manifest","select":{"by":"id","manifest_id":"m","actor":"me"}}"#
            )
            .is_err(),
            "an unknown field nested in a selector must not deserialize"
        );
    }

    #[test]
    fn compare_refuses_two_names_for_one_subject() {
        let same = ContextQuery::Compare {
            left: CompareSubject::Manifest {
                manifest_id: manifest_id(),
            },
            right: CompareSubject::Manifest {
                manifest_id: manifest_id(),
            },
            stages: ContextStage::ALL.to_vec(),
        };
        assert_eq!(
            same.validate(),
            Err(ContextWireError::CompareSubjectsIdentical)
        );

        let distinct = ContextQuery::Compare {
            left: CompareSubject::Manifest {
                manifest_id: manifest_id(),
            },
            right: CompareSubject::Run { run_id: run_id() },
            stages: ContextStage::ALL.to_vec(),
        };
        assert_eq!(distinct.validate(), Ok(()));
    }

    #[test]
    fn context_becomes_mandatory_at_v2_and_the_contract_number_does_not_move() {
        // R9: the flip is on the version axis alone. R10: the contract axis
        // stays where it is. Both in one assertion, because the failure mode is
        // someone moving them together.
        assert_eq!(CONTEXT_MANDATORY_FROM, ProtocolVersion::V2);
        assert_ne!(CONTEXT_MANDATORY_FROM, ProtocolVersion::V1);
        assert_eq!(gwk_domain::CONTRACT_VERSION, 1);
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
    fn the_read_grammar_is_eight_reads_and_the_count_is_pinned() {
        // The module doc says eight. Nothing asserted it, so "eight" was a
        // claim in prose next to a grammar that could have grown a ninth.
        let reads = [
            r#"{"query":"manifest","select":{"by":"id","manifest_id":"m"}}"#,
            r#"{"query":"supplement","manifest_id":"m","kind":"release","limit":5}"#,
            &format!(
                r#"{{"query":"source","manifest_id":"m","digest":"sha256:{}"}}"#,
                "a".repeat(64)
            ),
            r#"{"query":"provenance_graph","root":"m","depth":2}"#,
            &format!(
                r#"{{"query":"semantic_graph","root":"sha256:{}","depth":2}}"#,
                "a".repeat(64)
            ),
            r#"{"query":"execution_dag","run_id":"r","depth":2}"#,
            r#"{"query":"explain","manifest_id":"m","subject":{"subject":"precedence"}}"#,
            r#"{"query":"compare","left":{"of":"manifest","manifest_id":"m"},"right":{"of":"run","run_id":"r"},"stages":["resolved"]}"#,
        ];
        assert_eq!(reads.len(), 8, "the read grammar is eight reads");

        let mut decoded = Vec::new();
        for raw in reads {
            decoded.push(
                serde_json::from_str::<ContextQuery>(raw)
                    .unwrap_or_else(|e| panic!("{raw} did not decode: {e}")),
            );
        }
        // Every one of them a DISTINCT variant: eight documents that all decoded
        // to the same read would satisfy the count and prove nothing.
        for (i, left) in decoded.iter().enumerate() {
            for right in decoded.iter().skip(i + 1) {
                assert_ne!(
                    std::mem::discriminant(left),
                    std::mem::discriminant(right),
                    "two reads decoded to the same variant"
                );
            }
        }
    }

    #[test]
    fn a_comparison_must_name_stages_and_must_not_repeat_them() {
        let subjects = || {
            (
                CompareSubject::Manifest {
                    manifest_id: manifest_id(),
                },
                CompareSubject::Run { run_id: run_id() },
            )
        };
        let compare = |stages: Vec<ContextStage>| {
            let (left, right) = subjects();
            ContextQuery::Compare {
                left,
                right,
                stages,
            }
        };

        assert_eq!(
            compare(Vec::new()).validate(),
            Err(ContextWireError::CompareStagesOutOfRange)
        );
        assert_eq!(
            compare(vec![
                ContextStage::Resolved;
                CONTEXT_COMPARE_STAGE_MAX_COUNT + 1
            ])
            .validate(),
            Err(ContextWireError::CompareStagesOutOfRange)
        );
        assert_eq!(
            compare(vec![ContextStage::Resolved, ContextStage::Resolved]).validate(),
            Err(ContextWireError::CompareStagesRepeated)
        );
        // The positive control, and the bound's own edge: all five distinct
        // stages is the largest legal comparison.
        assert_eq!(compare(ContextStage::ALL.to_vec()).validate(), Ok(()));
        assert_eq!(ContextStage::ALL.len(), CONTEXT_COMPARE_STAGE_MAX_COUNT);
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
}
