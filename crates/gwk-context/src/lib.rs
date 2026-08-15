//! The GridWork Context Runtime's contract vocabulary.
//!
//! ADR-0032 makes Context a third public plane beside Authored and Execution:
//! the compiler resolves one immutable manifest per spawn attempt, an
//! independent verifier checks it, and Explain/Compare reconstruct what
//! happened from immutable projections. This crate holds the words all of that
//! agrees on — stages, truth records, participation, precedence, and content
//! digests.
//!
//! What is here is deliberately narrow. The compiler, verifier, CAS layer, and
//! wire-v2 grammar remain later 8A/8B work. This is the vocabulary and immutable
//! truth-record spine they will be written against.
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
//! The four truth records are generated public contract roots now. Publishing
//! those data shapes does not activate the wire-v2 grammar or change the kernel's
//! accepted protocol major.

pub mod digest;
pub mod manifest;
pub mod participation;
pub mod precedence;
pub mod skill;
pub mod stage;

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
