//! **This crate has no API.** Nothing is exported and there is nothing to import.
//!
//! It holds the short namespace prefix so the contract crates published under
//! `gwk-*` share one owned root. Those crates are where the work is:
//!
//! - [`gwk-domain`](https://docs.rs/gwk-domain) — shared domain types, events, and state machines
//! - [`gwk-cert`](https://docs.rs/gwk-cert) — conformance checker for an event stream
//! - [`gwk-theme`](https://docs.rs/gwk-theme) — the SIGNAL design tokens
//! - [`gwk-kernel`](https://docs.rs/gwk-kernel) — the daemon: event store, projections, blobs, attention, authority, the wire
//!
//! The terminal binary lives in [`gridwork`](https://docs.rs/gridwork). See <https://gridwork.sh>.
#![no_std]
