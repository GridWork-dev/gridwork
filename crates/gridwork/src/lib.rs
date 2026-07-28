//! GridWork — an agent operating system for the terminal.
//!
//! This crate is the published home of the forthcoming `gw` binary. It carries no
//! API yet: the current public surface is the contract stack, and this crate holds
//! the name so the binary and the contract ship under one identity.
//!
//! - [`gwk-domain`](https://docs.rs/gwk-domain) — shared domain types, events, and state machines
//! - [`gwk-cert`](https://docs.rs/gwk-cert) — conformance checker for an event stream
//! - [`gwk-theme`](https://docs.rs/gwk-theme) — the SIGNAL design tokens
//!
//! See <https://gridwork.dev>.
#![no_std]
