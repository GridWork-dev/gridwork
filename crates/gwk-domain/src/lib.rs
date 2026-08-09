//! The GridWork contract: shared domain types, event definitions, and state machines.
//!
//! Everything the kernel, clients, and adapters agree on lives here. The TypeScript
//! contract consumed by external tooling is generated from these types and CI-checked
//! against the committed artifact.

pub mod blob;
pub mod checkpoint;
pub mod command;
pub mod contract_sql;
pub mod engine;
pub mod entity;
pub mod envelope;
pub mod frame;
pub mod fsm;
pub mod ids;
pub mod ingestion;
pub mod inherited;
pub mod port;
pub mod protocol;
pub mod transition;

pub use blob::{
    BLOB_ADDRESS_SCHEME, BLOB_CHUNK_BYTES, BlobAddress, BlobAddressError, BlobDescriptor,
    SHA256_HEX_LEN, is_sha256_hex,
};
pub use checkpoint::{
    CHECKPOINT_EVENT_INTERVAL, CHECKPOINT_INTERVAL_SECS, CHECKPOINT_SCHEMA_VERSION, Checkpoint,
};
pub use command::{CommandDecodeError, KernelCommand};
// Deliberately NOT in the generated TypeScript contract: nothing in `engine`
// derives `specta::Type` and nothing is registered in xtask's export registry.
// This is how the host's own components agree with each other, not something a
// client parses off the wire.
pub use engine::{EngineAdapter, EngineEvent, EngineStatus, LifecycleFact};
pub use entity::{
    Attempt, AttentionItem, AuthorityGrant, Budget, Command, CostEntry,
    DISPATCH_NODE_INITIAL_STATE, DispatchNode, EngineSession, Evidence, Gate, IngestedRecord,
    Lease, Message, Receipt, Task, Worktree,
};
pub use envelope::{
    Actor, CommandEnvelope, ENVELOPE_SCHEMA_VERSION, EventEnvelope, INLINE_PAYLOAD_MAX_BYTES,
    Origin, PayloadRef, UnknownSchemaVersion, accept_schema_version,
};
pub use frame::{
    CellColor, CellStyle, CellUnderline, PtyAnsiSlot, PtyCellUpdate, PtyDelta, PtyFrame, StyledCell,
};
pub use fsm::{
    AttemptState, CommandState, GateVerdict, LeaseMode, LeaseState, MessageState, Outcome,
    StateMachine, TaskState,
};
pub use ids::{
    AggregateId, AttemptId, AttentionItemId, AuthorityGrantId, BlobUploadId, ByteCount, CommandId,
    CorrelationId, CostEntryId, CostMicros, DispatchNodeId, EngineId, EngineSessionId, EventCount,
    EventId, EvidenceId, FenceToken, GateId, IdempotencyKey, IngestedRecordId, LeaseId, MessageId,
    ProjectId, PtyFrameSeq, PtySessionGeneration, PtySessionId, ReceiptId, RequestId, Seq, TaskId,
    Timestamp, TokenCount, WorktreeId, WriterEpoch,
};
pub use ingestion::IngestionKind;
pub use inherited::{
    BudgetCursor, FindingAction, LeaseSnapshot, OpenAttemptRef, OrchestratorCheckpoint,
    PendingApproval, RoundFindingSummary,
};
pub use protocol::{
    CAPABILITY_NAME_MAX_BYTES, CONNECTION_EGRESS_BYTES_PER_WINDOW,
    CONNECTION_INGRESS_BYTES_PER_WINDOW, CONTRACT_VERSION, CapabilityName, CapabilityNameError,
    ClientControl, FRAME_BODY_MAX_BYTES, FRAME_BODY_MIN_BYTES, FRAME_KIND_RESERVED_STREAM,
    FRAME_LENGTH_PREFIX_BYTES, FrameKind, HELLO_DEADLINE_SECS, HELLO_MAX_BYTES, KernelErrorCode,
    KernelRequest, KernelResult, MAX_CAPABILITIES, PROTOCOL_MINOR, ProjectionKind,
    ProjectionRecord, ProtocolVersion, SLOW_CONSUMER_TIMEOUT_SECS, ServerControl,
};
pub use transition::{
    Cursor, GuardCtx, GuardViolation, LIVENESS_PRODUCER_KIND, TransitionGuard, TransitionRequest,
    TransitionResult, apply,
};
