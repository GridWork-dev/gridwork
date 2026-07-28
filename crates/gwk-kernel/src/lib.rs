//! The GridWork kernel: the PostgreSQL implementation behind the `gwk`
//! contract.
//!
//! `gwk-domain` says WHAT the contract is; this crate is one engine's answer to
//! it. The split is load-bearing — everything backend-specific (this crate, the
//! `gwk_internal` schema, the migrations beside it) stays on this side of the
//! line, and `schema/0001_contract.sql` stays engine-neutral so a second
//! backend has something to conform to.
//!
//! What is here so far is the scaffolding: configuration, the embedded
//! contract DDL and its fingerprint, and the one-shot initializer that applies
//! it and grants the runtime role. The append actor, projections, blob spine,
//! and wire server land on top of it.

pub mod admin;
pub mod config;
pub mod contract_sql;
pub mod error;

pub use admin::{InitOutcome, RoleAttributes, RuntimePrivileges, TargetState};
pub use config::{AdminConfig, DEFAULT_SOCKET_PATH, KernelConfig};
pub use contract_sql::{CONTRACT_SQL, CONTRACT_SQL_SHA256};
pub use error::{KernelError, Result};
