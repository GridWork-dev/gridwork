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
//! The asymmetry is the mitigation, so it is asserted rather than described:
//! `no_context_command_can_carry_attribution` walks every variant's serialized
//! form and fails if an actor-shaped key appears in any of them.
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
    Assurance, CONTEXT_ID_MAX_BYTES, ContextStage, Digest, EvidenceRefs, FinalizationSupplementId,
    ManifestId, ObservationIndex, ObservationSupplementId, ParticipationRecords, RecordCount,
    ReleaseSupplementId, TruthRecordError,
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
/// Maximum UTF-8 bytes in an optimization candidate's human-facing summary.
pub const CONTEXT_CANDIDATE_SUMMARY_MAX_BYTES: usize = 2_048;
/// Maximum rows one projection read may ask for.
pub const CONTEXT_QUERY_LIMIT_MAX: u32 = 1_000;
/// Maximum edge hops a graph read may traverse.
pub const CONTEXT_GRAPH_DEPTH_MAX: u32 = 16;
/// How many subjects a comparison takes. Compare is binary by definition:
/// three-way diff has no defined answer for "which one is the baseline".
pub const CONTEXT_COMPARE_SUBJECT_COUNT: usize = 2;

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
    /// A candidate summary exceeded its byte bound.
    CandidateSummaryTooLong,
    /// A projection read asked for zero rows or more than the bound allows.
    QueryLimitOutOfRange,
    /// A graph read asked for zero hops or more than the bound allows.
    GraphDepthOutOfRange,
    /// A comparison named the same subject twice.
    CompareSubjectsIdentical,
}

impl std::fmt::Display for ContextWireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::EmptyAttribution => "attribution component must not be empty",
            Self::AttributionTooLong => "attribution component exceeds its byte bound",
            Self::TooManyCandidateRoutes => "candidate declares too many affected routes",
            Self::CandidateSummaryTooLong => "candidate summary exceeds its byte bound",
            Self::QueryLimitOutOfRange => "projection limit must be nonzero and inside its bound",
            Self::GraphDepthOutOfRange => "graph depth must be nonzero and inside its bound",
            Self::CompareSubjectsIdentical => "comparison requires two distinct subjects",
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

/// The three `aggregate_type` families Context writes into the one kernel log.
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
pub enum ContextAggregate {
    /// Resolution and release of one immutable manifest.
    ContextManifest,
    /// The observed lifetime around one rendered manifest.
    ContextRun,
    /// Proposed and dispositioned optimization candidates.
    ContextOptimization,
}

impl ContextAggregate {
    pub const ALL: [Self; 3] = [
        Self::ContextManifest,
        Self::ContextRun,
        Self::ContextOptimization,
    ];

    /// The exact `EventEnvelope::aggregate_type` string.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ContextManifest => "context_manifest",
            Self::ContextRun => "context_run",
            Self::ContextOptimization => "context_optimization",
        }
    }
}

impl std::fmt::Display for ContextAggregate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The ten D4 lifecycle `event_type` values.
///
/// Ten because ADR-0032 decision 4 names ten, and the count is pinned by test
/// rather than left to whoever next reads the list. Verification and rejection
/// are ONE name carrying a verdict, which is how the ADR names them too — the
/// alternative spends a name on a field and makes "was it verified?" a question
/// about which of two event types arrived.
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
// Spelled out per variant rather than derived by `rename_all`, because the
// derived spelling is the VARIANT name and the wire needs the aggregate-prefixed
// one. `rename_all = "snake_case"` emitted `compilation_requested` into the
// generated TypeScript while `as_str` emitted
// `context_manifest_compilation_requested` — one value under two names, the
// envelope carrying one and the payload beside it carrying the other.
pub enum ContextEventName {
    #[serde(rename = "context_manifest_compilation_requested")]
    CompilationRequested,
    #[serde(rename = "context_manifest_resolved")]
    ManifestResolved,
    #[serde(rename = "context_manifest_verification_recorded")]
    ManifestVerificationRecorded,
    #[serde(rename = "context_manifest_release_recorded")]
    ReleaseRecorded,
    #[serde(rename = "context_run_opened")]
    RunOpened,
    #[serde(rename = "context_run_observation_appended")]
    ObservationAppended,
    #[serde(rename = "context_run_closed")]
    RunClosed,
    #[serde(rename = "context_run_assurance_certified")]
    AssuranceCertified,
    #[serde(rename = "context_optimization_candidate_proposed")]
    OptimizationCandidateProposed,
    #[serde(rename = "context_optimization_candidate_dispositioned")]
    OptimizationCandidateDispositioned,
}

impl ContextEventName {
    pub const ALL: [Self; 10] = [
        Self::CompilationRequested,
        Self::ManifestResolved,
        Self::ManifestVerificationRecorded,
        Self::ReleaseRecorded,
        Self::RunOpened,
        Self::ObservationAppended,
        Self::RunClosed,
        Self::AssuranceCertified,
        Self::OptimizationCandidateProposed,
        Self::OptimizationCandidateDispositioned,
    ];

    /// The exact `EventEnvelope::event_type` string.
    ///
    /// Prefixed by its aggregate, matching `task` / `task_state_changed` in the
    /// existing log. The prefix is not decoration: `event_type` is globally
    /// open, so an unprefixed `run_closed` would collide with the first other
    /// aggregate that closes a run.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::CompilationRequested => "context_manifest_compilation_requested",
            Self::ManifestResolved => "context_manifest_resolved",
            Self::ManifestVerificationRecorded => "context_manifest_verification_recorded",
            Self::ReleaseRecorded => "context_manifest_release_recorded",
            Self::RunOpened => "context_run_opened",
            Self::ObservationAppended => "context_run_observation_appended",
            Self::RunClosed => "context_run_closed",
            Self::AssuranceCertified => "context_run_assurance_certified",
            Self::OptimizationCandidateProposed => "context_optimization_candidate_proposed",
            Self::OptimizationCandidateDispositioned => {
                "context_optimization_candidate_dispositioned"
            }
        }
    }

    /// The aggregate family this event belongs to.
    pub const fn aggregate(self) -> ContextAggregate {
        match self {
            Self::CompilationRequested
            | Self::ManifestResolved
            | Self::ManifestVerificationRecorded
            | Self::ReleaseRecorded => ContextAggregate::ContextManifest,
            Self::RunOpened
            | Self::ObservationAppended
            | Self::RunClosed
            | Self::AssuranceCertified => ContextAggregate::ContextRun,
            Self::OptimizationCandidateProposed | Self::OptimizationCandidateDispositioned => {
                ContextAggregate::ContextOptimization
            }
        }
    }
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
        affected_route_count: RecordCount,
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
        if let Self::Compare { left, right, .. } = self
            && left == right
        {
            return Err(ContextWireError::CompareSubjectsIdentical);
        }
        Ok(())
    }
}

/// Reject an oversized affected-route declaration.
///
/// A free function because the count rides a `RecordCount` inside a fact
/// variant, and the bound is a Context-plane rule rather than a property of
/// counts in general.
pub fn check_candidate_routes(count: RecordCount) -> Result<(), ContextWireError> {
    if count.value() as usize > CONTEXT_CANDIDATE_ROUTE_MAX_COUNT {
        return Err(ContextWireError::TooManyCandidateRoutes);
    }
    Ok(())
}

/// Reject an oversized candidate summary.
pub fn check_candidate_summary(summary: &str) -> Result<(), ContextWireError> {
    if summary.len() > CONTEXT_CANDIDATE_SUMMARY_MAX_BYTES {
        return Err(ContextWireError::CandidateSummaryTooLong);
    }
    Ok(())
}

/// Reject an identifier that would not fit the record-id bound.
///
/// Re-exported reasoning rather than a second rule: wire identifiers and truth
/// record identifiers are the same identifiers, so they share one bound.
pub fn check_wire_id(value: &str) -> Result<(), TruthRecordError> {
    if value.len() > CONTEXT_ID_MAX_BYTES {
        return Err(TruthRecordError::IdTooLong);
    }
    Ok(())
}

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
                affected_route_count: RecordCount::new(1).expect("valid count"),
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
    fn candidate_bounds_reject_past_their_edge() {
        assert_eq!(
            check_candidate_routes(
                RecordCount::new(CONTEXT_CANDIDATE_ROUTE_MAX_COUNT as u32).expect("valid")
            ),
            Ok(())
        );
        assert_eq!(
            check_candidate_routes(
                RecordCount::new(CONTEXT_CANDIDATE_ROUTE_MAX_COUNT as u32 + 1).expect("valid")
            ),
            Err(ContextWireError::TooManyCandidateRoutes)
        );
        assert_eq!(
            check_candidate_summary(&"a".repeat(CONTEXT_CANDIDATE_SUMMARY_MAX_BYTES + 1)),
            Err(ContextWireError::CandidateSummaryTooLong)
        );
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
