//! The storage ports: what any event-store and blob backend must provide.
//!
//! The contract keeps storage engine-neutral — engine-specific mechanics
//! (queues, notifications, locks) live in a backend crate behind these traits
//! and in deployment docs, never in contract semantics. A backend proves
//! itself by passing `gwk-cert`'s conformance suite, not by being the first
//! implementation.
//!
//! Ordering contract: `global_sequence` is assigned inside `append` in COMMIT
//! order by the store's single append actor — unique, strictly increasing,
//! NOT gapless. Client-supplied `global_sequence`/`appended_at` values on
//! input envelopes are ignored and overwritten.

use crate::blob::{BlobAddress, BlobDescriptor};
use crate::envelope::EventEnvelope;
use crate::ids::{BlobUploadId, ByteCount, EvidenceId, FenceToken, Seq};

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

/// Why a blob operation was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlobError {
    /// No blob at that address, or no such upload.
    NotFound,
    /// Crypto-shredded: the wrapped DEK is gone. This is PERMANENT and is
    /// answered even while ciphertext cleanup is still pending, so a delayed
    /// sweep never turns a shredded blob back into a readable one.
    Tombstoned,
    /// The committed plaintext did not hash to the address the caller claimed.
    DigestMismatch {
        expected: BlobAddress,
        actual: BlobAddress,
    },
    /// AEAD authentication, container header, or chunk-length verification
    /// failed — tampering or truncation, not a decode preference.
    Integrity(String),
    /// Still pinned as evidence; sweep and shred must not touch it.
    Pinned,
    /// Backend failure, opaque to the contract.
    Storage(String),
}

impl std::fmt::Display for BlobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str("blob not found"),
            Self::Tombstoned => f.write_str("blob tombstoned: the wrapped key is gone"),
            Self::DigestMismatch { expected, actual } => {
                write!(f, "digest mismatch: expected {expected}, got {actual}")
            }
            Self::Integrity(reason) => write!(f, "blob integrity failure: {reason}"),
            Self::Pinned => f.write_str("blob is pinned as evidence"),
            Self::Storage(reason) => write!(f, "storage error: {reason}"),
        }
    }
}

impl std::error::Error for BlobError {}

/// A content-addressed store for payloads too large to inline.
///
/// Addresses are digests over PLAINTEXT, so deduplication works across
/// deployments while encryption stays a storage concern (ADR 0003).
/// Implementations never accept a caller-supplied path: the validated digest
/// alone determines where bytes land.
pub trait BlobStore {
    /// Start an upload. The caller declares what it intends to write; the
    /// store mints the id and owns the temporary file.
    fn begin(
        &self,
        media_type: String,
        byte_size: ByteCount,
    ) -> impl Future<Output = Result<BlobUploadId, BlobError>>;

    /// Append one plaintext chunk. `sequence` is contiguous from `0`; a gap or
    /// a repeat is refused rather than reordered.
    fn write_chunk(
        &self,
        upload: &BlobUploadId,
        sequence: u32,
        chunk: &[u8],
    ) -> impl Future<Output = Result<(), BlobError>>;

    /// Finish the upload, requiring the plaintext to hash to `address`.
    ///
    /// Returns the descriptor and whether an identical blob (digest, size, AND
    /// media type) already existed — a dedup hit writes nothing new.
    fn commit(
        &self,
        upload: BlobUploadId,
        address: BlobAddress,
    ) -> impl Future<Output = Result<(BlobDescriptor, bool), BlobError>>;

    /// Discard an upload and its temporary file. Uncommitted uploads also
    /// expire on their own after an hour.
    fn abort(&self, upload: BlobUploadId) -> impl Future<Output = Result<(), BlobError>>;

    /// Read a bounded range of plaintext.
    fn read(
        &self,
        address: &BlobAddress,
        offset: ByteCount,
        length: ByteCount,
    ) -> impl Future<Output = Result<Vec<u8>, BlobError>>;

    /// Describe a committed blob. `Ok(None)` means it never existed;
    /// [`BlobError::Tombstoned`] means it did and was shredded — a distinction
    /// a retention audit needs.
    fn stat(
        &self,
        address: &BlobAddress,
    ) -> impl Future<Output = Result<Option<BlobDescriptor>, BlobError>>;

    /// Pin as evidence, blocking sweep until every pin is released.
    fn pin(
        &self,
        address: &BlobAddress,
        evidence: &EvidenceId,
    ) -> impl Future<Output = Result<(), BlobError>>;

    fn unpin(
        &self,
        address: &BlobAddress,
        evidence: &EvidenceId,
    ) -> impl Future<Output = Result<(), BlobError>>;

    /// Remove unreferenced, unpinned blobs; returns what it removed.
    fn sweep(&self) -> impl Future<Output = Result<Vec<BlobAddress>, BlobError>>;

    /// Crypto-shred: commit the tombstone and drop the wrapped DEK FIRST, then
    /// remove ciphertext. Ordered that way so a crash mid-shred leaves an
    /// unreadable blob, never a readable one.
    fn shred(&self, address: &BlobAddress) -> impl Future<Output = Result<(), BlobError>>;
}
