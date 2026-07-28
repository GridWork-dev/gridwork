//! The GridWork contract: shared domain types, event definitions, and state machines.
//!
//! Everything the kernel, clients, and adapters agree on lives here. The TypeScript
//! contract consumed by external tooling is generated from these types and CI-checked
//! against the committed artifact.

pub mod envelope;
pub mod ids;

pub use envelope::{
    Actor, CommandEnvelope, ENVELOPE_SCHEMA_VERSION, EventEnvelope, INLINE_PAYLOAD_MAX_BYTES,
    Origin, PayloadRef, UnknownSchemaVersion, accept_schema_version,
};
pub use ids::{
    AggregateId, AttemptId, AttentionItemId, AuthorityGrantId, ByteCount, CommandId, CorrelationId,
    CostMicros, DispatchNodeId, EngineId, EngineSessionId, EventId, EvidenceId, FenceToken, GateId,
    IdempotencyKey, LeaseId, MessageId, ProjectId, ReceiptId, Seq, TaskId, Timestamp, WorktreeId,
};
