//! The GridWork Context Runtime's contract vocabulary.
//!
//! ADR-0032 makes Context a third public plane beside Authored and Execution:
//! the compiler resolves one immutable manifest per spawn attempt, an
//! independent verifier checks it, and Explain/Compare reconstruct what
//! happened from immutable projections. This crate holds the words all of that
//! agrees on — stages, truth records, participation, precedence, and content
//! digests.
//!
//! What is here is deliberately narrow. The compiler, verifier, and CAS layer
//! remain later 8A/8B work. This is the vocabulary, the immutable truth-record
//! spine, and — since E3 — the wire-v2 grammar they will be written against.
//!
//! The grammar being present is not the grammar being live. [`wire`] publishes
//! shapes for a protocol major that [`gwk_domain::ProtocolVersion`] names and
//! the kernel refuses; freezing it under review before anything speaks it is
//! the reason to publish it early rather than alongside its first caller.
//!
//! ## Rulings this crate encodes
//!
//! The design forks behind these shapes are ruled in the phase's kickoff
//! record. Summarized where a reader would otherwise wonder:
//!
//! - **[`Digest`] is not [`gwk_domain::BlobAddress`]** (R2/F2). Same
//!   validation, different meaning: one names content, the other locates bytes
//!   in the encrypted CAS.
//! - **Truth stage is implicit, and there are five of them** (R4/F4). No
//!   record carries a stage field; [`ContextStage`] exists so surfaces agree
//!   on the words. ADR-0032 names three levels in one place and five stages
//!   across two others — the ruling settles it at five rather than letting
//!   each consumer guess.
//! - **[`ParticipationState`] is a plain enum, not a state machine** (R5/F6).
//!   A resolved manifest is immutable, so participation is classified once and
//!   never transitioned.
//! - **[`ParticipationReason`] is closed** (R6/F3). Explain/Compare branches on
//!   it across thousands of manifests; open strings work for `Gate.kind` only
//!   because nothing branches on that.
//!
//! The four truth records and the wire-v2 grammar are generated public contract
//! roots. Publishing those data shapes does not change the kernel's accepted
//! protocol major, and `CONTRACT_VERSION` stays `1` — the wire major and the
//! domain contract are separate axes, and only the first of them moved (R10).

pub mod digest;
pub mod manifest;
pub mod participation;
pub mod precedence;
pub mod skill;
pub mod stage;
pub mod wire;

pub use digest::{DIGEST_SCHEME, Digest, DigestError};
pub use manifest::{
    Assurance, CONTEXT_EVIDENCE_MAX_COUNT, CONTEXT_ID_MAX_BYTES,
    CONTEXT_PARTICIPATION_DETAIL_MAX_BYTES, CONTEXT_PARTICIPATION_MAX_COUNT,
    CONTEXT_RECORD_COUNT_MAX, EvidenceRefs, FinalizationSupplement, FinalizationSupplementId,
    ManifestId, ObservationIndex, ObservationSupplement, ObservationSupplementId,
    ParticipationRecords, RecordCount, ReleaseSupplement, ReleaseSupplementId, ResolvedManifest,
    TruthRecordError,
};
pub use participation::{
    Participation, ParticipationError, ParticipationReason, ParticipationState,
};
pub use precedence::{Contribution, PrecedenceConflict, PrecedenceTier, resolve};
pub use stage::ContextStage;
pub use wire::{
    AttributionPart, CONTEXT_ATTRIBUTION_MAX_BYTES, CONTEXT_CANDIDATE_ROUTE_MAX_COUNT,
    CONTEXT_COMPARE_STAGE_MAX_COUNT, CONTEXT_GRAPH_DEPTH_MAX, CONTEXT_MANDATORY_FROM,
    CONTEXT_QUERY_LIMIT_MAX, CandidateDisposition, CompareSubject, ContextAggregate,
    ContextAttribution, ContextEventName, ContextEventPayload, ContextFact, ContextQuery,
    ContextRunId, ContextWireError, ExplainSubject, GraphDepth, ManifestSelector,
    OptimizationCandidateId, QueryLimit, RecordContextFact, RouteCount, SupplementKind,
    VerificationVerdict,
};
