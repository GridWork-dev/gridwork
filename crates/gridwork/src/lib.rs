//! **This crate has no API.** Nothing is exported and there is nothing to import.
//!
//! It is the published home of the forthcoming `gw` binary — a name reservation so
//! the binary and the contract ship under one identity. `cargo install gridwork`
//! installs nothing useful until that binary lands.
//!
//! GridWork is an agent operating system for the terminal. The public surface today
//! is the contract stack:
//!
//! - [`gwk-domain`](https://docs.rs/gwk-domain) — shared domain types, events, and state machines
//! - [`gwk-cert`](https://docs.rs/gwk-cert) — conformance checker for an event stream
//! - [`gwk-theme`](https://docs.rs/gwk-theme) — the SIGNAL design tokens
//!
//! See <https://gridwork.dev>.
#![no_std]
