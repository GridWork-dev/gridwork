//! The GridWork kernel namespace — the umbrella for the `gwk-*` contract crates.
//!
//! This crate carries no API yet. It holds the short namespace prefix so the
//! contract crates published under `gwk-*` share one owned root:
//!
//! - [`gwk-domain`](https://docs.rs/gwk-domain) — shared domain types, events, and state machines
//! - [`gwk-cert`](https://docs.rs/gwk-cert) — conformance checker for an event stream
//! - [`gwk-theme`](https://docs.rs/gwk-theme) — the SIGNAL design tokens
//!
//! The terminal binary lives in [`gridwork`](https://docs.rs/gridwork). See <https://gridwork.dev>.
#![no_std]
