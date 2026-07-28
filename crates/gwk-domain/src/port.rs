//! The storage port: what any event-store backend must provide.
//!
//! The contract keeps storage engine-neutral — engine-specific mechanics
//! (queues, notifications, locks) live in a backend crate behind this trait
//! and in deployment docs, never in contract semantics. A backend proves
//! itself by passing `gwk-cert`'s conformance suite, not by being the first
//! implementation.
//!
//! Ordering contract: `global_sequence` is assigned inside `append` in COMMIT
//! order by the store's single append actor — unique, strictly increasing,
//! NOT gapless. Client-supplied `global_sequence`/`appended_at` values on
//! input envelopes are ignored and overwritten.

use crate::envelope::EventEnvelope;
use crate::ids::{FenceToken, Seq};

/// Why an append was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppendError {
    /// CAS refusal: the aggregate's current version was not `expected`.
    VersionConflict { actual: u32, expected: u32 },
    /// A stale fence token was presented (an append actor lost its lease).
    Fenced {
        presented: FenceToken,
        current: FenceToken,
    },
    /// The batch itself is malformed (mixed aggregates, non-contiguous
    /// versions, empty).
    MalformedBatch(String),
    /// Backend failure, opaque to the contract.
    Storage(String),
}

impl std::fmt::Display for AppendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::VersionConflict { actual, expected } => {
                write!(f, "version conflict: actual {actual}, expected {expected}")
            }
            Self::Fenced { presented, current } => {
                write!(f, "fenced: presented {presented}, current {current}")
            }
            Self::MalformedBatch(reason) => write!(f, "malformed batch: {reason}"),
            Self::Storage(reason) => write!(f, "storage error: {reason}"),
        }
    }
}

impl std::error::Error for AppendError {}

/// A read/watermark failure, opaque to the contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageError(pub String);

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "storage error: {}", self.0)
    }
}

impl std::error::Error for StorageError {}

/// Ceiling on a single `read_from` page. Conforming stores CLAMP `limit`
/// to this value — a larger request returns at most this many events, it is
/// not an error. The write path bounds inline payload bytes; this bounds the
/// read path so no caller can demand an unbounded page.
pub const MAX_READ_LIMIT: usize = 65_536;

/// An append-only, commit-ordered event store.
pub trait EventStore {
    /// Atomically append one aggregate's batch.
    ///
    /// * All events must target the same `(aggregate_type, aggregate_id)`,
    ///   with contiguous `aggregate_version`s starting at `expected_version + 1`.
    /// * `expected_version` is the CAS precondition (`0` = new aggregate).
    /// * `fence`, when the store has granted tokens, must be the CURRENT one.
    /// * On success the returned envelopes carry the assigned
    ///   `global_sequence` and `appended_at`.
    fn append(
        &self,
        expected_version: u32,
        fence: Option<FenceToken>,
        events: Vec<EventEnvelope>,
    ) -> impl Future<Output = Result<Vec<EventEnvelope>, AppendError>>;

    /// Read committed events with `global_sequence` strictly after `cursor`
    /// (`None` = from the beginning), ascending, at most `limit` (clamped to
    /// [`MAX_READ_LIMIT`]).
    ///
    /// This is the recovery path: a consumer that lost wakeups re-reads from
    /// its durable cursor and misses nothing.
    fn read_from(
        &self,
        cursor: Option<Seq>,
        limit: usize,
    ) -> impl Future<Output = Result<Vec<EventEnvelope>, StorageError>>;

    /// The highest committed `global_sequence`, if any event exists.
    fn watermark(&self) -> impl Future<Output = Result<Option<Seq>, StorageError>>;

    /// Grant/rotate the append fence. Each grant returns a token strictly
    /// greater than every earlier one and invalidates them.
    fn grant_fence(&self) -> impl Future<Output = Result<FenceToken, StorageError>>;
}
