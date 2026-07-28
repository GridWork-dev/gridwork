//! Generic contract-conformance checking: given any event stream, verify state-machine
//! legality (transitions, CAS versioning, adjacency) against the `gwk-domain` contract.
//!
//! This crate is deployment-agnostic — it knows nothing about any particular database,
//! host, or operator. Migration- or site-specific certification tooling belongs to the
//! consumer, not here.
//!
//! Two halves:
//! - [`conformance`] — the trait-generic STORAGE suite a backend crate
//!   instantiates against its `EventStore` implementation.
//! - the STREAM checker (the `gwk-cert` CLI) — validates any exported event
//!   stream against the contract.

pub mod conformance;
