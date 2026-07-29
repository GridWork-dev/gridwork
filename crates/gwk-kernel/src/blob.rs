//! Content-addressed blob storage: the encrypted container and the store above
//! it.
//!
//! The address is a digest over PLAINTEXT (ADR 0003), so two deployments with
//! different KEKs still agree on what a blob IS and deduplication survives a
//! key rotation. Encryption is purely a storage concern.
//!
//! [`container`] is the byte format and nothing else — no filesystem, no
//! database, no clock. That is what lets its golden vectors be exact.

pub mod container;
