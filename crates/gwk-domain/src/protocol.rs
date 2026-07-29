//! The client↔kernel wire protocol, as types.
//!
//! Frames are `[u32 big-endian body_length][u8 frame_kind][body]` (ADR 0001).
//! `body_length` INCLUDES the kind byte and EXCLUDES the four-byte prefix, so a
//! reader bounds its allocation before it parses anything. Kind `0x01` carries
//! exactly one UTF-8 JSON control value: a [`ClientControl`] or a
//! [`ServerControl`].
//!
//! **Scope of this module.** It defines the frame bounds and the control
//! grammar. It does NOT decode frames: ADR 0001 requires a *validating*
//! decoder for recursive duplicate keys, unknown fields, invalid UTF-8, and
//! trailing bytes, because serde alone cannot refuse a duplicate key nested
//! three levels down. The strict decoder that enforces those refusals lives in
//! the kernel's wire layer and answers with the [`KernelErrorCode`] values
//! declared here. Consequently a `serde` round trip of these types is
//! necessary but not sufficient for wire acceptance.
//!
//! The same split applies to the COUNT bounds: [`MAX_CAPABILITIES`] and
//! [`FRAME_BODY_MAX_BYTES`] are declared here and enforced by that decoder.
//! Types whose value set is closed ([`CapabilityName`],
//! [`ProtocolVersion`], [`BlobAddress`], [`KernelErrorCode`]) DO validate
//! themselves at `Deserialize` time, so a value of those types is legal
//! wherever it came from.

use crate::blob::{BlobAddress, BlobDescriptor};
use crate::entity::{
    Attempt, AttentionItem, AuthorityGrant, Command, DispatchNode, EngineSession, Evidence, Gate,
    IngestedRecord, Lease, Message, Receipt, Task, Worktree,
};
use crate::envelope::{CommandEnvelope, EventEnvelope, JsonValue};
use crate::ids::{
    BlobUploadId, ByteCount, CommandId, EventCount, EventId, RequestId, Seq, WriterEpoch,
};
use crate::inherited::OrchestratorCheckpoint;

// ============================================================
// Versions and bounds
// ============================================================

/// The protocol MAJOR version. It must match EXACTLY — an unknown major is a
/// typed refusal, never a best-effort session (ADR 0001).
///
/// On the wire it is a plain JSON number: the value is small and bounded, so
/// the decimal-string rule for 64-bit counters does not apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProtocolVersion {
    V1,
}

impl ProtocolVersion {
    pub const fn as_u32(self) -> u32 {
        match self {
            Self::V1 => 1,
        }
    }

    pub const fn from_u32(value: u32) -> Option<Self> {
        match value {
            1 => Some(Self::V1),
            _ => None,
        }
    }
}

impl std::fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_u32())
    }
}

impl serde::Serialize for ProtocolVersion {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u32(self.as_u32())
    }
}

impl<'de> serde::Deserialize<'de> for ProtocolVersion {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = u32::deserialize(d)?;
        Self::from_u32(raw).ok_or_else(|| {
            serde::de::Error::custom(format!("unsupported protocol major version {raw}"))
        })
    }
}

impl specta::Type for ProtocolVersion {
    fn definition(types: &mut specta::Types) -> specta::datatype::DataType {
        <u32 as specta::Type>::definition(types)
    }
}

/// The protocol MINOR version this crate speaks. Minor negotiates DOWNWARD:
/// both sides use `min(client, server)`, so a newer peer degrades instead of
/// refusing.
pub const PROTOCOL_MINOR: u32 = 0;

/// The version of the DOMAIN CONTRACT this crate defines — the event, command,
/// and projection shapes — which is a different axis from the wire protocol
/// above. The genesis payload and every status response report this same
/// number, so both read it here rather than each inventing one.
pub const CONTRACT_VERSION: u32 = 1;

/// Bytes of the big-endian length prefix, excluded from `body_length`.
pub const FRAME_LENGTH_PREFIX_BYTES: usize = 4;

/// Smallest legal `body_length` — the kind byte alone. Zero is a refusal, so a
/// peer can never announce an empty frame.
pub const FRAME_BODY_MIN_BYTES: u32 = 1;

/// Largest legal `body_length`, kind byte included. Checked BEFORE allocation.
pub const FRAME_BODY_MAX_BYTES: u32 = 4 * 1024 * 1024;

/// The first frame must be a hello no larger than this.
pub const HELLO_MAX_BYTES: u32 = 64 * 1024;

/// A connection that has not sent its hello within this many seconds is closed.
pub const HELLO_DEADLINE_SECS: u64 = 5;

/// Byte-accounted per-connection ingress budget.
pub const CONNECTION_INGRESS_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Byte-accounted per-connection egress budget.
pub const CONNECTION_EGRESS_MAX_BYTES: usize = 8 * 1024 * 1024;

/// A subscriber blocked this long is disconnected with its last delivered
/// cursor, so it can resume rather than restart.
pub const SLOW_CONSUMER_TIMEOUT_SECS: u64 = 30;

/// Most capability names either side may offer or grant.
pub const MAX_CAPABILITIES: usize = 64;

/// Longest capability name, in bytes.
pub const CAPABILITY_NAME_MAX_BYTES: usize = 64;

// Relations between the bounds, checked at COMPILE time in every build — a
// later edit that makes the hello cap exceed the frame cap, or the blob chunk
// size unshippable once base64 expands it (4 bytes out per 3 in), fails to
// build rather than failing in production.
const _: () = {
    assert!(HELLO_MAX_BYTES < FRAME_BODY_MAX_BYTES);
    assert!(FRAME_BODY_MIN_BYTES >= 1);
    assert!(crate::blob::BLOB_CHUNK_BYTES.div_ceil(3) * 4 < FRAME_BODY_MAX_BYTES as usize);
};

/// The kind byte of a frame. Not a JSON type — it never appears inside a body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum FrameKind {
    /// Exactly one UTF-8 JSON control value.
    Json = 0x01,
}

/// Reserved for the 5′ terminal engine's raw bytes. It is neither advertised
/// nor accepted in v1 — reserving the number here is what stops v1 from
/// spending it on something else.
pub const FRAME_KIND_RESERVED_STREAM: u8 = 0x02;

impl FrameKind {
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Decode a kind byte. The reserved 5′ kind and every unknown byte are the
    /// same answer: `None`, refused before any body is parsed.
    pub const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0x01 => Some(Self::Json),
            _ => None,
        }
    }
}

// ============================================================
// Capability names
// ============================================================

/// Why a capability name was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityNameError {
    Empty,
    TooLong,
    /// Not `[a-z][a-z0-9_]*` without a trailing underscore.
    NotSnakeCase,
}

impl std::fmt::Display for CapabilityNameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => f.write_str("capability name is empty"),
            Self::TooLong => write!(
                f,
                "capability name exceeds {CAPABILITY_NAME_MAX_BYTES} bytes"
            ),
            Self::NotSnakeCase => {
                f.write_str("capability name must be snake_case: [a-z][a-z0-9_]*, no trailing _")
            }
        }
    }
}

impl std::error::Error for CapabilityNameError {}

/// A negotiated capability name: an OPEN set (new capabilities are additive),
/// but each name is strictly shaped so two spellings can never mean one thing.
///
/// Capability negotiation is NOT authorization — being granted a name says the
/// peer will accept those requests, not that any particular one is permitted.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct CapabilityName(String);

impl CapabilityName {
    pub fn new(value: impl AsRef<str>) -> Result<Self, CapabilityNameError> {
        let value = value.as_ref();
        if value.is_empty() {
            return Err(CapabilityNameError::Empty);
        }
        if value.len() > CAPABILITY_NAME_MAX_BYTES {
            return Err(CapabilityNameError::TooLong);
        }
        let bytes = value.as_bytes();
        let head_ok = bytes[0].is_ascii_lowercase();
        let body_ok = bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_');
        let tail_ok = bytes[bytes.len() - 1] != b'_';
        if !(head_ok && body_ok && tail_ok) {
            return Err(CapabilityNameError::NotSnakeCase);
        }
        Ok(Self(value.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CapabilityName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for CapabilityName {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        Self::new(raw.as_ref()).map_err(serde::de::Error::custom)
    }
}

// The WIRE form is a string, so the exported TS type is `string`.
impl specta::Type for CapabilityName {
    fn definition(types: &mut specta::Types) -> specta::datatype::DataType {
        <String as specta::Type>::definition(types)
    }
}

// ============================================================
// Errors
// ============================================================

/// The stable refusal set. Errors are VALUES in this contract: a client
/// branches on the code and never parses a message.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    specta::Type,
)]
#[serde(rename_all = "snake_case")]
pub enum KernelErrorCode {
    /// The handshake did not complete: no hello, a late hello, or a
    /// non-hello first frame.
    Handshake,
    /// Major version mismatch.
    UnsupportedVersion,
    /// A capability was malformed, over the count/length bound, or required
    /// and not granted.
    Capability,
    /// The request did not satisfy the contract's own shape rules.
    Validation,
    /// A duplicate JSON key at any nesting depth.
    DuplicateKey,
    /// `body_length` was zero, over the bound, or the kind byte unknown.
    FrameSize,
    /// The kernel is sealed and this request is not on the sealed allowlist.
    Sealed,
    /// The kernel is already active and this request is activation-only.
    AlreadyActive,
    NotFound,
    /// CAS refusal; the actual version travels in the error detail.
    StaleVersion,
    /// The requested edge is not in the state machine's table.
    IllegalEdge,
    /// The actor is not permitted; the kernel raised an attention item or
    /// denied outright.
    Authority,
    /// The idempotency key was reused with different canonical request content.
    IdempotencyConflict,
    /// A stale writer epoch or fence token was presented.
    Fenced,
    /// Bounded queues are full.
    Overloaded,
    /// A subscriber fell too far behind and was disconnected.
    SlowConsumer,
    /// Backend failure, opaque to the contract.
    Storage,
    /// An unreadable `schema_version` with no upcaster.
    Schema,
    /// The runtime database role holds privileges the kernel refuses to run
    /// with, or lacks ones it needs.
    Privilege,
    /// A blob failed authentication, digest, or length verification.
    BlobIntegrity,
    /// The blob was crypto-shredded; its reads fail permanently.
    BlobTombstoned,
}

impl KernelErrorCode {
    /// Every code, in declaration order.
    pub const ALL: &'static [Self] = &[
        Self::Handshake,
        Self::UnsupportedVersion,
        Self::Capability,
        Self::Validation,
        Self::DuplicateKey,
        Self::FrameSize,
        Self::Sealed,
        Self::AlreadyActive,
        Self::NotFound,
        Self::StaleVersion,
        Self::IllegalEdge,
        Self::Authority,
        Self::IdempotencyConflict,
        Self::Fenced,
        Self::Overloaded,
        Self::SlowConsumer,
        Self::Storage,
        Self::Schema,
        Self::Privilege,
        Self::BlobIntegrity,
        Self::BlobTombstoned,
    ];

    /// The wire value — identical to what serde emits.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Handshake => "handshake",
            Self::UnsupportedVersion => "unsupported_version",
            Self::Capability => "capability",
            Self::Validation => "validation",
            Self::DuplicateKey => "duplicate_key",
            Self::FrameSize => "frame_size",
            Self::Sealed => "sealed",
            Self::AlreadyActive => "already_active",
            Self::NotFound => "not_found",
            Self::StaleVersion => "stale_version",
            Self::IllegalEdge => "illegal_edge",
            Self::Authority => "authority",
            Self::IdempotencyConflict => "idempotency_conflict",
            Self::Fenced => "fenced",
            Self::Overloaded => "overloaded",
            Self::SlowConsumer => "slow_consumer",
            Self::Storage => "storage",
            Self::Schema => "schema",
            Self::Privilege => "privilege",
            Self::BlobIntegrity => "blob_integrity",
            Self::BlobTombstoned => "blob_tombstoned",
        }
    }
}

impl std::fmt::Display for KernelErrorCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ============================================================
// Projections
// ============================================================

/// Which projection a get/list request names. CLOSED: the projection set is
/// the contract's own, so an unknown name is a refusal, not an empty page.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    specta::Type,
)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionKind {
    Task,
    Attempt,
    EngineSession,
    Message,
    Command,
    Gate,
    AuthorityGrant,
    Receipt,
    Evidence,
    AttentionItem,
    Worktree,
    Lease,
    DispatchNode,
    OrchestratorCheckpoint,
    IngestedRecord,
}

impl ProjectionKind {
    /// Every projection, in declaration order.
    pub const ALL: &'static [Self] = &[
        Self::Task,
        Self::Attempt,
        Self::EngineSession,
        Self::Message,
        Self::Command,
        Self::Gate,
        Self::AuthorityGrant,
        Self::Receipt,
        Self::Evidence,
        Self::AttentionItem,
        Self::Worktree,
        Self::Lease,
        Self::DispatchNode,
        Self::OrchestratorCheckpoint,
        Self::IngestedRecord,
    ];
}

/// One returned projection row, tagged by `projection_type`.
///
/// A tagged union, not a bare JSON object: ADR 0001 forbids a client inferring
/// which record it got from which fields happen to be present.
///
/// Every variant holds exactly ONE field, named for its tag — so a reader
/// never has to remember a per-variant key. `record_field_name_equals_the_tag`
/// enforces it.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(tag = "projection_type", rename_all = "snake_case")]
pub enum ProjectionRecord {
    Task {
        task: Task,
    },
    Attempt {
        attempt: Attempt,
    },
    EngineSession {
        engine_session: EngineSession,
    },
    Message {
        message: Message,
    },
    Command {
        command: Command,
    },
    Gate {
        gate: Gate,
    },
    AuthorityGrant {
        authority_grant: AuthorityGrant,
    },
    Receipt {
        receipt: Receipt,
    },
    Evidence {
        evidence: Evidence,
    },
    AttentionItem {
        attention_item: AttentionItem,
    },
    Worktree {
        worktree: Worktree,
    },
    Lease {
        lease: Lease,
    },
    DispatchNode {
        dispatch_node: DispatchNode,
    },
    OrchestratorCheckpoint {
        orchestrator_checkpoint: OrchestratorCheckpoint,
    },
    IngestedRecord {
        ingested_record: IngestedRecord,
    },
}

impl ProjectionRecord {
    /// Which projection this row belongs to — the value a `get`/`list` request
    /// must have named.
    pub const fn kind(&self) -> ProjectionKind {
        match self {
            Self::Task { .. } => ProjectionKind::Task,
            Self::Attempt { .. } => ProjectionKind::Attempt,
            Self::EngineSession { .. } => ProjectionKind::EngineSession,
            Self::Message { .. } => ProjectionKind::Message,
            Self::Command { .. } => ProjectionKind::Command,
            Self::Gate { .. } => ProjectionKind::Gate,
            Self::AuthorityGrant { .. } => ProjectionKind::AuthorityGrant,
            Self::Receipt { .. } => ProjectionKind::Receipt,
            Self::Evidence { .. } => ProjectionKind::Evidence,
            Self::AttentionItem { .. } => ProjectionKind::AttentionItem,
            Self::Worktree { .. } => ProjectionKind::Worktree,
            Self::Lease { .. } => ProjectionKind::Lease,
            Self::DispatchNode { .. } => ProjectionKind::DispatchNode,
            Self::OrchestratorCheckpoint { .. } => ProjectionKind::OrchestratorCheckpoint,
            Self::IngestedRecord { .. } => ProjectionKind::IngestedRecord,
        }
    }
}

// ============================================================
// Requests
// ============================================================

/// What a client asks for. Tagged by `type`.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KernelRequest {
    /// Liveness only. Served sealed and active.
    Health,
    /// Full readiness detail. Served sealed and active.
    Status,
    /// How far the log goes, without a subscription. Served sealed and active.
    Watermark,
    /// Prove the fresh epoch: exactly one genesis event, zero business rows.
    /// Served sealed and active.
    VerifySealed,
    /// The only mutation path. Sealed kernels admit `activate_kernel` alone.
    SubmitCommand {
        envelope: CommandEnvelope,
    },
    GetProjection {
        projection: ProjectionKind,
        id: String,
    },
    ListProjection {
        projection: ProjectionKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        cursor: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        limit: Option<u32>,
    },
    /// One page of events strictly after `cursor` (absent = from the
    /// beginning). `limit` is CLAMPED, not refused
    /// ([`MAX_READ_LIMIT`](crate::port::MAX_READ_LIMIT)).
    ReadEvents {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        cursor: Option<Seq>,
        limit: u32,
    },
    /// Durable-cursor subscription. Delivery is at-least-once; a reconnect
    /// from the last cursor recovers everything, in order.
    SubscribeEvents {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        cursor: Option<Seq>,
    },
    BlobBegin {
        media_type: String,
        byte_size: ByteCount,
    },
    /// One plaintext chunk, base64 in a bounded control frame — blob bytes
    /// never ride an ordinary payload (ADR 0001). Chunks are
    /// [`BLOB_CHUNK_BYTES`](crate::blob::BLOB_CHUNK_BYTES) of plaintext, whose
    /// base64 expansion stays inside [`FRAME_BODY_MAX_BYTES`].
    BlobChunk {
        upload_id: BlobUploadId,
        /// Contiguous from 0; a gap or repeat is a refusal.
        sequence: u32,
        data_base64: String,
    },
    /// Finish an upload. The kernel recomputes the plaintext digest and
    /// refuses if it is not `address`.
    BlobCommit {
        upload_id: BlobUploadId,
        address: BlobAddress,
    },
    BlobAbort {
        upload_id: BlobUploadId,
    },
    BlobRead {
        address: BlobAddress,
        offset: ByteCount,
        length: ByteCount,
    },
    BlobStat {
        address: BlobAddress,
    },
}

// ============================================================
// Results
// ============================================================

/// What one request answered with, tagged by `type`. [`KernelResult::Error`]
/// is an ordinary variant: a refusal is a value, not an out-of-band condition.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum KernelResult {
    Health {
        ready: bool,
        sealed: bool,
    },
    Status {
        sealed: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        watermark: Option<Seq>,
        /// The durable writer epoch this process booted into.
        writer_epoch: WriterEpoch,
        /// Always [`CONTRACT_VERSION`] — the same number the genesis payload
        /// carries, so the two can be compared without a translation table.
        contract_version: u32,
        /// The full 40-character public revision this binary was built from.
        public_revision: String,
    },
    Watermark {
        /// Absent means an empty log — never `0`, which is a real sequence.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        watermark: Option<Seq>,
    },
    /// The fresh-epoch proof. `genesis_watermark` is DATABASE-ASSIGNED and is
    /// never assumed to be `1` (ADR 0026).
    SealedVerification {
        sealed: bool,
        genesis_event_id: EventId,
        genesis_watermark: Seq,
        /// Exactly `1` in a fresh sealed epoch — a COUNT, unrelated to the
        /// sequence above.
        event_count: EventCount,
    },
    /// The command was applied. `events` are the appended envelopes with their
    /// assigned sequences; an idempotent replay answers with the original ones.
    CommandApplied {
        command_id: CommandId,
        events: Vec<EventEnvelope>,
        watermark: Seq,
    },
    Projection {
        record: ProjectionRecord,
    },
    ProjectionPage {
        records: Vec<ProjectionRecord>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        next_cursor: Option<String>,
    },
    Events {
        events: Vec<EventEnvelope>,
        /// The last delivered sequence; absent when the page was empty.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        cursor: Option<Seq>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        watermark: Option<Seq>,
    },
    /// The subscription is live; batches follow as [`ServerControl::EventBatch`].
    Subscribed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        cursor: Option<Seq>,
    },
    BlobBegun {
        upload_id: BlobUploadId,
    },
    BlobChunkAccepted {
        upload_id: BlobUploadId,
        sequence: u32,
    },
    BlobCommitted {
        descriptor: BlobDescriptor,
        /// True when an identical digest, size, AND media type already
        /// existed, so nothing new was written (ADR 0003).
        deduplicated: bool,
    },
    BlobAborted {
        upload_id: BlobUploadId,
    },
    BlobBytes {
        address: BlobAddress,
        offset: ByteCount,
        data_base64: String,
    },
    BlobStat {
        descriptor: BlobDescriptor,
    },
    Error {
        code: KernelErrorCode,
        message: String,
        /// Machine-readable specifics the code alone cannot carry — the actual
        /// version behind a `stale_version`, the offending field behind a
        /// `validation`. Absent when the code says everything.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional, type = Option<JsonValue>)]
        detail: Option<serde_json::Value>,
    },
}

// ============================================================
// Control frames
// ============================================================

/// Everything a client may send on kind `0x01`.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientControl {
    /// The mandatory first frame. Nothing else is decoded before it succeeds.
    Hello {
        protocol_major: ProtocolVersion,
        protocol_minor: u32,
        /// At most [`MAX_CAPABILITIES`] names the client would like.
        capabilities: Vec<CapabilityName>,
        /// Free-form client label, for logs only. Never an identity: the UDS
        /// peer credential is the only authentication (ADR 0002).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        client: Option<String>,
    },
    Request {
        request_id: RequestId,
        request: KernelRequest,
    },
}

/// Everything the kernel may send on kind `0x01`.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerControl {
    HelloAck {
        protocol_major: ProtocolVersion,
        /// `min(client_minor, server_minor)`.
        protocol_minor: u32,
        /// The INTERSECTION of what the client asked for and what this kernel
        /// offers. A client must not assume anything absent here.
        capabilities: Vec<CapabilityName>,
        sealed: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        watermark: Option<Seq>,
    },
    HelloRefusal {
        code: KernelErrorCode,
        message: String,
    },
    Response {
        request_id: RequestId,
        result: KernelResult,
    },
    /// One batch on a live subscription. `cursor` is the last sequence in the
    /// batch — what a reconnect resumes from.
    EventBatch {
        request_id: RequestId,
        events: Vec<EventEnvelope>,
        cursor: Seq,
    },
    /// A subscription ended. `last_cursor` is what the consumer actually
    /// received, so a slow-consumer disconnect resumes without a gap.
    StreamClosed {
        request_id: RequestId,
        code: KernelErrorCode,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[specta(optional)]
        last_cursor: Option<Seq>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_major_is_exact_and_unknown_majors_are_refused() {
        assert_eq!(ProtocolVersion::V1.as_u32(), 1);
        assert_eq!(ProtocolVersion::from_u32(1), Some(ProtocolVersion::V1));
        assert_eq!(ProtocolVersion::from_u32(0), None);
        assert_eq!(ProtocolVersion::from_u32(2), None);

        // A plain JSON number on the wire, and an unknown major is a typed
        // refusal at DECODE time — not a best-effort session.
        assert_eq!(
            serde_json::to_value(ProtocolVersion::V1).expect("serialize"),
            serde_json::json!(1)
        );
        assert!(serde_json::from_value::<ProtocolVersion>(serde_json::json!(2)).is_err());
    }

    #[test]
    fn frame_bounds_match_the_accepted_codec() {
        assert_eq!(FRAME_LENGTH_PREFIX_BYTES, 4);
        assert_eq!(FRAME_BODY_MIN_BYTES, 1);
        assert_eq!(FRAME_BODY_MAX_BYTES, 4_194_304);
        assert_eq!(HELLO_MAX_BYTES, 65_536);
        assert_eq!(CONNECTION_INGRESS_MAX_BYTES, 8 * 1024 * 1024);
        assert_eq!(CONNECTION_EGRESS_MAX_BYTES, 8 * 1024 * 1024);
        assert_eq!(SLOW_CONSUMER_TIMEOUT_SECS, 30);
        assert_eq!(HELLO_DEADLINE_SECS, 5);
        assert_eq!(MAX_CAPABILITIES, 64);
        // The relations between these bounds are pinned at compile time by the
        // `const _` block beside their declarations.
    }

    #[test]
    fn frame_kind_admits_json_only_and_reserves_the_5p_byte() {
        assert_eq!(FrameKind::Json.as_u8(), 0x01);
        assert_eq!(FrameKind::from_u8(0x01), Some(FrameKind::Json));
        // Reserved for 5′: refused in v1 exactly like an unknown byte.
        assert_eq!(FrameKind::from_u8(FRAME_KIND_RESERVED_STREAM), None);
        assert_eq!(FrameKind::from_u8(0x00), None);
        assert_eq!(FrameKind::from_u8(0xFF), None);
    }

    #[test]
    fn capability_names_are_strict_snake_case() {
        assert!(CapabilityName::new("event_subscribe").is_ok());
        assert!(CapabilityName::new("blob").is_ok());
        assert!(CapabilityName::new("v2_read").is_ok());
        for bad in [
            "",
            "EventSubscribe",
            "event-subscribe",
            "event subscribe",
            "_leading",
            "trailing_",
            "1numeric",
            "event.subscribe",
            "événement",
        ] {
            assert!(CapabilityName::new(bad).is_err(), "accepted {bad}");
        }
        assert!(CapabilityName::new("a".repeat(CAPABILITY_NAME_MAX_BYTES)).is_ok());
        assert!(CapabilityName::new("a".repeat(CAPABILITY_NAME_MAX_BYTES + 1)).is_err());
        // Validation binds the wire path too, not just construction.
        assert!(serde_json::from_str::<CapabilityName>("\"EventSubscribe\"").is_err());
        let ok: CapabilityName = serde_json::from_str("\"event_subscribe\"").expect("decode");
        assert_eq!(ok.as_str(), "event_subscribe");
    }

    #[test]
    fn error_codes_are_unique_snake_case_and_agree_with_serde() {
        for code in KernelErrorCode::ALL {
            assert_eq!(
                serde_json::to_value(code).expect("serialize"),
                serde_json::json!(code.as_str()),
                "as_str() disagrees with serde for {code:?}"
            );
            assert!(
                code.as_str()
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{code:?} is not snake_case"
            );
        }
        let mut wire: Vec<&str> = KernelErrorCode::ALL.iter().map(|c| c.as_str()).collect();
        let total = wire.len();
        wire.sort_unstable();
        wire.dedup();
        assert_eq!(wire.len(), total, "two error codes share a wire value");
        assert_eq!(total, 21);
        assert_eq!(
            KernelErrorCode::IdempotencyConflict.as_str(),
            "idempotency_conflict"
        );
    }

    #[test]
    fn record_field_name_equals_the_tag() {
        // One field per variant, named for the tag: a client that read the tag
        // already knows the key, and no variant can quietly grow a second one.
        for record in sample_records() {
            let json = serde_json::to_value(&record).expect("serialize");
            let object = json.as_object().expect("record is an object");
            let tag = object["projection_type"].as_str().expect("tagged");
            let fields: Vec<&str> = object
                .keys()
                .map(String::as_str)
                .filter(|k| *k != "projection_type")
                .collect();
            assert_eq!(fields, [tag], "projection {tag} has fields {fields:?}");
        }
    }

    #[test]
    fn every_projection_kind_has_exactly_one_record_variant() {
        // The get/list request names a ProjectionKind and the answer carries a
        // ProjectionRecord; a kind with no record is a request that can never
        // be answered.
        let records = sample_records();
        assert_eq!(records.len(), ProjectionKind::ALL.len());
        let kinds: Vec<ProjectionKind> = records.iter().map(ProjectionRecord::kind).collect();
        assert_eq!(kinds, ProjectionKind::ALL.to_vec());
        for record in &records {
            let json = serde_json::to_value(record).expect("serialize");
            let tag = json["projection_type"].as_str().expect("tagged");
            let kind_wire = serde_json::to_value(record.kind()).expect("serialize kind");
            assert_eq!(serde_json::json!(tag), kind_wire);
            let back: ProjectionRecord = serde_json::from_value(json).expect("deserialize");
            assert_eq!(&back, record);
        }
    }

    #[test]
    fn control_frames_round_trip_with_their_type_tags() {
        let hello = ClientControl::Hello {
            protocol_major: ProtocolVersion::V1,
            protocol_minor: PROTOCOL_MINOR,
            capabilities: vec![
                CapabilityName::new("event_subscribe").expect("valid"),
                CapabilityName::new("blob").expect("valid"),
            ],
            client: None,
        };
        let json = serde_json::to_value(&hello).expect("serialize");
        assert_eq!(json["type"], "hello");
        assert_eq!(json["protocol_major"], 1);
        assert!(json.get("client").is_none(), "absent optional was emitted");
        assert_eq!(
            serde_json::from_value::<ClientControl>(json).expect("deserialize"),
            hello
        );

        let watermark = ServerControl::Response {
            request_id: RequestId::new("req-1"),
            result: KernelResult::Watermark {
                watermark: Some(Seq::new(9_007_199_254_740_993)),
            },
        };
        let json = serde_json::to_value(&watermark).expect("serialize");
        assert_eq!(json["type"], "response");
        assert_eq!(json["result"]["type"], "watermark");
        // > 2^53 survives because the counter is a decimal STRING.
        assert_eq!(json["result"]["watermark"], "9007199254740993");
        assert_eq!(
            serde_json::from_value::<ServerControl>(json).expect("deserialize"),
            watermark
        );

        // An empty log answers with an ABSENT watermark, never 0 — which is a
        // real sequence a store could have assigned.
        let empty =
            serde_json::to_value(KernelResult::Watermark { watermark: None }).expect("serialize");
        assert_eq!(empty, serde_json::json!({ "type": "watermark" }));
    }

    #[test]
    fn a_refusal_is_a_value_on_the_ordinary_response_path() {
        let refused = ServerControl::Response {
            request_id: RequestId::new("req-2"),
            result: KernelResult::Error {
                code: KernelErrorCode::StaleVersion,
                message: "version conflict".into(),
                detail: Some(serde_json::json!({ "actual": 7, "expected": 6 })),
            },
        };
        let json = serde_json::to_value(&refused).expect("serialize");
        assert_eq!(json["result"]["code"], "stale_version");
        assert_eq!(json["result"]["detail"]["actual"], 7);
        assert_eq!(
            serde_json::from_value::<ServerControl>(json).expect("deserialize"),
            refused
        );
    }

    #[test]
    fn unit_requests_carry_only_their_tag() {
        for (request, tag) in [
            (KernelRequest::Health, "health"),
            (KernelRequest::Status, "status"),
            (KernelRequest::Watermark, "watermark"),
            (KernelRequest::VerifySealed, "verify_sealed"),
        ] {
            let json = serde_json::to_value(&request).expect("serialize");
            assert_eq!(json, serde_json::json!({ "type": tag }));
            assert_eq!(
                serde_json::from_value::<KernelRequest>(json).expect("deserialize"),
                request
            );
        }
    }

    /// One record per [`ProjectionKind`], in the same order.
    fn sample_records() -> Vec<ProjectionRecord> {
        use crate::envelope::Actor;
        use crate::fsm::{
            AttemptState, CommandState, GateVerdict, LeaseMode, LeaseState, MessageState, TaskState,
        };
        use crate::ids::{
            AttemptId, AttentionItemId, AuthorityGrantId, DispatchNodeId, EngineId,
            EngineSessionId, EvidenceId, GateId, IdempotencyKey, IngestedRecordId, LeaseId,
            MessageId, ReceiptId, TaskId, Timestamp, WorktreeId,
        };

        let ts = || Timestamp::new("2026-07-28T00:00:00Z");
        let actor = || Actor {
            kind: "kernel".into(),
            id: None,
        };
        vec![
            ProjectionRecord::Task {
                task: Task {
                    id: TaskId::new("task-1"),
                    version: 1,
                    state: TaskState::Submitted,
                    kind: None,
                    title: None,
                    spec_ref: None,
                    project: None,
                    priority: None,
                    tracker_ref: None,
                    created_at: ts(),
                    updated_at: ts(),
                },
            },
            ProjectionRecord::Attempt {
                attempt: Attempt {
                    id: AttemptId::new("att-1"),
                    version: 1,
                    state: AttemptState::Queued,
                    task_id: TaskId::new("task-1"),
                    engine: EngineId::new("engine-a"),
                    capability: None,
                    role: None,
                    model_lane: None,
                    permission_profile: None,
                    worktree_lease_id: None,
                    base_sha: None,
                    budget: None,
                    result_schema_ref: None,
                    provider_session_ref: None,
                    runtime_ref: None,
                    runtime_started_at: None,
                    exit_code: None,
                    provider_terminal_event: None,
                    result_valid: None,
                    evidence_manifest_ref: None,
                    gate_result: None,
                    created_at: ts(),
                    updated_at: ts(),
                },
            },
            ProjectionRecord::EngineSession {
                engine_session: EngineSession {
                    id: EngineSessionId::new("sess-1"),
                    attempt_id: AttemptId::new("att-1"),
                    engine: EngineId::new("engine-a"),
                    provider_session_ref: None,
                    started_at: ts(),
                    ended_at: None,
                },
            },
            ProjectionRecord::Message {
                message: Message {
                    id: MessageId::new("msg-1"),
                    version: 1,
                    state: MessageState::Accepted,
                    idempotency_key: IdempotencyKey::new("send-once"),
                    correlation_id: None,
                    reply_to: None,
                    sender: None,
                    recipient: None,
                    channel: None,
                    kind: None,
                    payload: None,
                    deadline: None,
                    delivery_attempts: 0,
                    dead_letter_reason: None,
                    delivery_refs: None,
                    created_at: ts(),
                    updated_at: ts(),
                },
            },
            ProjectionRecord::Command {
                command: Command {
                    id: CommandId::new("cmd-1"),
                    version: 1,
                    state: CommandState::Issued,
                    kind: "stop_attempt".into(),
                    target: None,
                    actor: None,
                    idempotency_key: None,
                    outcome: None,
                    created_at: ts(),
                    updated_at: ts(),
                },
            },
            ProjectionRecord::Gate {
                gate: Gate {
                    id: GateId::new("gate-1"),
                    version: 1,
                    attempt_id: None,
                    phase_ref: None,
                    kind: None,
                    verdict: GateVerdict::Pending,
                    evidence_ref: None,
                    created_at: ts(),
                    updated_at: ts(),
                },
            },
            ProjectionRecord::AuthorityGrant {
                authority_grant: AuthorityGrant {
                    id: AuthorityGrantId::new("grant-1"),
                    grantee: actor(),
                    action_class: "deploy".into(),
                    scope: None,
                    granted_at: ts(),
                    expires_at: None,
                    revoked_at: None,
                    revoke_reason: None,
                    receipt_id: None,
                },
            },
            ProjectionRecord::Receipt {
                receipt: Receipt {
                    id: ReceiptId::new("r-1"),
                    actor: actor(),
                    action: "state_flip".into(),
                    subject_type: "attempt".into(),
                    subject_id: "att-1".into(),
                    from: None,
                    to: None,
                    observed_basis: None,
                    ts: ts(),
                },
            },
            ProjectionRecord::Evidence {
                evidence: Evidence {
                    id: EvidenceId::new("ev-1"),
                    kind: "transcript".into(),
                    r#ref: "blob://transcript".into(),
                    digest: None,
                    byte_size: None,
                    created_at: ts(),
                },
            },
            ProjectionRecord::AttentionItem {
                attention_item: AttentionItem {
                    id: AttentionItemId::new("att-item-1"),
                    kind: "risk_tag".into(),
                    summary: "data-migration pages".into(),
                    subject_ref: None,
                    raised_by: None,
                    raised_at: ts(),
                    resolved_at: None,
                    resolution: None,
                },
            },
            ProjectionRecord::Worktree {
                worktree: Worktree {
                    id: WorktreeId::new("wt-1"),
                    repo: "gridwork".into(),
                    path: "/w/kernel".into(),
                    branch: "feature/kernel".into(),
                    base_sha: None,
                    lease_id: None,
                    dirty: false,
                    unpushed: false,
                    released_at: None,
                    disposition: None,
                    created_at: ts(),
                },
            },
            ProjectionRecord::Lease {
                lease: Lease {
                    id: LeaseId::new("lease-1"),
                    version: 1,
                    state: LeaseState::Held,
                    mode: LeaseMode::Exclusive,
                    holder: None,
                    scope: None,
                    repo: None,
                    path: None,
                    branch: None,
                    base_sha: None,
                    fence_token: None,
                    heartbeat_at: None,
                    expires_at: None,
                    dirty: false,
                    unpushed: false,
                    disposition: None,
                    created_at: ts(),
                    updated_at: ts(),
                },
            },
            ProjectionRecord::DispatchNode {
                dispatch_node: DispatchNode {
                    id: DispatchNodeId::new("node-1"),
                    version: 1,
                    parent_id: None,
                    attempt_id: None,
                    kind: "orchestrator".into(),
                    state: crate::entity::DISPATCH_NODE_INITIAL_STATE.into(),
                    label: None,
                    created_at: ts(),
                    updated_at: ts(),
                },
            },
            ProjectionRecord::OrchestratorCheckpoint {
                orchestrator_checkpoint: OrchestratorCheckpoint {
                    orchestrator_id: None,
                    seq: Seq::new(1),
                    native_session_ref: None,
                    active_goal: None,
                    active_step_ref: None,
                    latest_command_ref: None,
                    open_attempts: None,
                    leases: None,
                    pending_approvals: None,
                    budget_cursor: None,
                },
            },
            ProjectionRecord::IngestedRecord {
                ingested_record: IngestedRecord {
                    id: IngestedRecordId::new("ingest:proj-1:key-1"),
                    kind: crate::ingestion::IngestionKind::Memory,
                    payload: serde_json::json!({ "text": "recalled" }),
                    payload_ref: None,
                    ingested_by: None,
                    event_seq: Seq::new(7),
                    ingested_at: ts(),
                },
            },
        ]
    }
}
