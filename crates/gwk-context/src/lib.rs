//! The GridWork Context Runtime's contract vocabulary.
//!
//! ADR-0032 makes Context a third public plane beside Authored and Execution:
//! the compiler resolves one immutable manifest per spawn attempt, an
//! independent verifier checks it, and Explain/Compare reconstruct what
//! happened from immutable projections. This crate holds the words all of that
//! agrees on — stages, truth records, participation, precedence, and content
//! digests.
//!
//! What is here is deliberately narrow: the vocabulary, the immutable
//! truth-record spine, the storage read port, and — since E3 — the wire-v2
//! grammar the rest is written against. The compiler and the verifier are NOT
//! here, and their absence is the design rather than a stage of it. Each is its
//! own crate — [`gwk-context-compiler`] and [`gwk-context-verifier`] — because
//! R15 puts the verifier's independence in the dependency graph, where it is
//! enforced, instead of in a module boundary, where it would be remembered.
//! That leaves this crate as the one thing both are allowed to share.
//!
//! [`gwk-context-compiler`]: https://github.com/GridWork-dev/gridwork/tree/main/crates/gwk-context-compiler
//! [`gwk-context-verifier`]: https://github.com/GridWork-dev/gridwork/tree/main/crates/gwk-context-verifier
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

pub mod precedence;
pub mod skill;
pub mod stage;
pub mod store;
pub mod wire;

// The lifecycle WRITE grammar and the truth records it projects into live in
// `gwk-domain`, re-exported here so every caller keeps its current spelling.
// The move is a packaging consequence, not a design one: `gwk-kernel` is
// published and this crate is not, `cargo package` refuses that dependency in
// both directions, and the kernel is the process that deserializes these facts
// off the log. E8 set the precedent with the Context CAS port.
pub use gwk_domain::{
    Assurance, AttributionPart, CONTEXT_ATTRIBUTION_MAX_BYTES, CONTEXT_CANDIDATE_ROUTE_MAX_COUNT,
    CONTEXT_EVIDENCE_MAX_COUNT, CONTEXT_ID_MAX_BYTES, CONTEXT_PARTICIPATION_DETAIL_MAX_BYTES,
    CONTEXT_PARTICIPATION_MAX_COUNT, CONTEXT_RECORD_COUNT_MAX, CandidateDisposition,
    ContextAggregate, ContextAttribution, ContextEventName, ContextEventPayload, ContextFact,
    ContextRunId, ContextWireError, DIGEST_SCHEME, Digest, DigestError, EvidenceRefs,
    FinalizationSupplement, FinalizationSupplementId, ManifestId, ObservationIndex,
    ObservationSupplement, ObservationSupplementId, OptimizationCandidateId, Participation,
    ParticipationError, ParticipationReason, ParticipationRecords, ParticipationState,
    RecordContextFact, RecordCount, ReleaseSupplement, ReleaseSupplementId, ResolvedManifest,
    RouteCount, TruthRecordError, VerificationVerdict,
};
pub use precedence::{Contribution, PrecedenceConflict, PrecedenceTier};
pub use stage::ContextStage;
pub use store::ContextTruthStore;
pub use wire::{
    CONTEXT_COMPARE_STAGE_MAX_COUNT, CONTEXT_GRAPH_DEPTH_MAX, CONTEXT_MANDATORY_FROM,
    CONTEXT_QUERY_LIMIT_MAX, CompareSubject, ContextQuery, ExplainSubject, GraphDepth,
    ManifestSelector, QueryLimit, SupplementKind,
};
