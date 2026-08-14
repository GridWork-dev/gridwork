//! The GridWork Context Runtime's contract vocabulary.
//!
//! ADR-0032 makes Context a third public plane beside Authored and Execution:
//! the compiler resolves one immutable manifest per spawn attempt, an
//! independent verifier checks it, and Explain/Compare reconstruct what
//! happened from immutable projections. This crate holds the words all of that
//! agrees on — stages, participation, precedence, and content digests.
//!
//! What is here is deliberately narrow. The compiler, the verifier, the CAS
//! layer, the supplement entities, and the wire-v2 grammar are the rest of 8A
//! and 8B. This is the vocabulary they will be written in, landed first so
//! they cannot each invent their own.
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
//! ## Not here yet
//!
//! No `specta::Type` derives and no contract-root registration: these types
//! have not crossed a wire, and generating TypeScript for them would publish a
//! shape as settled when its grammar (F7–F9) is still open. They join the
//! generated contract when the wire does.

pub mod digest;
pub mod participation;
pub mod precedence;
pub mod stage;

pub use digest::{DIGEST_SCHEME, Digest, DigestError};
pub use participation::{
    Participation, ParticipationError, ParticipationReason, ParticipationState,
};
pub use precedence::{Contribution, PrecedenceConflict, PrecedenceTier, resolve};
pub use stage::ContextStage;
