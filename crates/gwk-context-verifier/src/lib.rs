//! The independent Context verifier.
//!
//! One resolved manifest and its supplements, checked by code that did not
//! build them. The goal clause this crate exists to satisfy is stated as an
//! absolute: *the manifest is checked by code that did not build it, and
//! compiler success is nowhere accepted as verification evidence.*
//!
//! # Why a crate and not a module
//!
//! D7 requires the verifier be implemented separately from the compiler. R15
//! sets that bar at a separate crate, because a crate boundary is the only
//! version of "separate" a dependency graph *enforces* — a module boundary
//! asks review discipline to remember, every time, forever.
//!
//! The bar is not decorative. An earlier shape put the compiler inside
//! `gwk-context`, the one crate the verifier is allowed to depend on, and that
//! arrangement separated the verifier from nothing precedence-shaped: the
//! resolver was re-exported at the root, so a guard matching `compile::` or
//! `precedence::resolve` was defeated by the ordinary spelling
//! `gwk_context::resolve`. Moving the compiler and the resolver into
//! `gwk-context-compiler` is what makes the boundary mean something: this crate
//! does not depend on that one, so no spelling reaches either.
//!
//! What that leaves shared is `gwk-context`'s public types — the truth records,
//! the [`gwk_context::Digest`] newtype, the precedence *vocabulary* — plus one
//! declared crypto primitive. The precedence types are shared and the resolver
//! is not, which is the line R15 draws: this crate can name a conflict, and
//! cannot ask the compiler what the answer was.
//!
//! # What is genuinely independent here, and what is not
//!
//! Worth stating plainly, because a verifier that overclaims is worse than one
//! that checks less.
//!
//! **Independent.** Every rule is re-derived from the canonical form as the
//! compiler's documentation *states* it, not as its code implements it. The
//! digest preimage rule, the participation ordering, the meaning of
//! `source_count` — each is written here a second time. If the two readings
//! disagree, the suite reds, which is the whole return on writing it twice.
//!
//! **Shared, and deliberately.** Both sides serialize through the same derived
//! `Serialize` on the same struct, so the two agree on field order by
//! construction. That is the type's own definition rather than either side's
//! logic, and re-deriving it would mean this crate maintaining a second copy
//! of a shape whose drift the contract tests already catch.
//!
//! **Not checked at all.** `source_bytes` is not recomputable from the
//! manifest: the record carries the total but not the per-candidate byte counts
//! it was summed from, so nothing here can tell a correct total from a wrong
//! one. It is named rather than quietly skipped — a reader is entitled to know
//! which fields carry no second opinion.

pub mod verify;

pub use verify::{
    MANIFEST_DIGEST_PLACEHOLDER_HEX, Package, VerifyError, cited_evidence, manifest_digest, verify,
    verify_manifest,
};
