//! The GridWork kernel: the PostgreSQL implementation behind the `gwk`
//! contract.
//!
//! `gwk-domain` says WHAT the contract is; this crate is one engine's answer to
//! it. The split is load-bearing — everything backend-specific (this crate, the
//! `gwk_internal` schema, the migrations beside it) stays on this side of the
//! line, and `schema/0001_contract.sql` stays engine-neutral so a second
//! backend has something to conform to.
//!
//! What is here so far: configuration, the embedded contract DDL and its
//! fingerprint, the one-shot initializer, the fenced append store, the epoch
//! that seals a fresh log until it is activated, the work lifecycle's command
//! path and projections, and the blob spine — the encrypted container format
//! and the store that dedups, pins, sweeps, rotates and shreds it, the
//! projection snapshots, and the recovery that decides what a restart may claim
//! about them. The wire server lands on top of them, and the TTL sweep rides
//! beside it — the kernel's own periodic answer to a lease-holder that died
//! rather than released, and to an attempt no live lease is backing at all.
//! Beside the embedded DDL sits the migration chain: the
//! authored steps that carry a database from one contract digest to the next,
//! and the resolver that decides whether this build knows a route between two
//! of them.

pub mod admin;
pub mod authority;
pub mod blob;
pub mod checkpoint;
pub mod config;
pub mod epoch;
pub mod error;
pub mod migrate;
pub mod numeric;
pub mod project;
pub mod recover;
pub mod store;
pub mod submit;
pub mod ttl_sweep;
pub mod wire;
pub mod writer;

pub use admin::{InitOutcome, RoleAttributes, RuntimePrivileges, TargetState};
pub use blob::store::{PgBlobStore, UPLOAD_EXPIRY_SECS};
pub use config::{AdminConfig, BlobConfig, DEFAULT_SOCKET_PATH, KernelConfig};
pub use epoch::{GENESIS_KEY, KERNEL_AGGREGATE, KERNEL_SINGLETON, SYSTEM_PROJECT};
pub use error::{KernelError, Result};
// The DDL is contract, so it is generated into `gwk-domain` and re-exported here
// — the paths every caller already uses keep resolving, and the crate that
// asserts the SQL matches its own state machines carries the bytes it asserts
// against. See `xtask/src/schema.rs` for why a constant and not `include_str!`.
pub use gwk_domain::contract_sql;
pub use gwk_domain::contract_sql::{CONTRACT_SQL, CONTRACT_SQL_SHA256};
// The migration steps travel the same road for the same reason, and land beside
// the DDL they amend. See `xtask/src/steps.rs` for the file format they are
// generated from.
pub use gwk_domain::contract_steps;
pub use gwk_domain::contract_steps::{CONTRACT_STEPS, Step};
pub use migrate::{ChainRefusal, resolve};
pub use project::Refusal;
pub use recover::{RebuildReport, RecoveryReport, Verdict};
pub use store::{EVENT_CHANNEL, MAX_INFLIGHT_APPENDS, PgEventStore, connect_pool};
pub use wire::WireError;
pub use wire::hello::{OFFERED_CAPABILITIES, Readiness, Session};
pub use writer::{WRITER_LOCK_KEY, WriterLock, claim_epoch};
