//! The GridWork Context Runtime's deterministic compiler.
//!
//! ADR-0032 puts context compilation after route and authority resolution and
//! before spawn: one immutable resolved manifest per attempt, checked by code
//! that did not build it. This crate is the "build it" half. The vocabulary it
//! speaks — tiers, participation, truth records, digests — is `gwk-context`;
//! the checking half is its own crate (ruling R15).
//!
//! ## Why the resolver lives here and not in `gwk-context`
//!
//! R15 makes the verifier a separate crate because a dependency graph is the
//! only version of "separate" that is enforced rather than remembered. With the
//! precedence resolver inside `gwk-context` — the one crate the verifier may
//! depend on — that boundary would separate the verifier from nothing
//! precedence-shaped: `gwk_context::resolve` was re-exported at the root, and a
//! name-match guard on any one spelling is defeated by another. So the resolver
//! IMPLEMENTATION lives in [`precedence`] here, beside its first non-test
//! caller, while the precedence TYPES (`PrecedenceTier`, `Contribution`,
//! `PrecedenceConflict`) stay in `gwk-context` as shared vocabulary. The
//! verifier's manifest never names this crate, so no spelling reaches either.
//!
//! ## What this crate does not do
//!
//! No I/O. Inputs are typed values ([`compile::CompileRequest`],
//! [`compile::Route`], [`compile::Authority`], [`compile::Candidate`]); the
//! caller loads candidates through the Task 7 storage ports and hands over
//! values. No wire and no DDL: the output is the already-registered
//! `ResolvedManifest`, and the attribution derived from it (ruling R12).

pub mod compile;
pub mod precedence;

pub use compile::{
    Authority, COMPILER, Candidate, CompileError, CompileRequest, Compiled,
    MANIFEST_DIGEST_PLACEHOLDER_HEX, Route, Standing, attribution, compile, manifest_digest,
};
pub use precedence::resolve;
