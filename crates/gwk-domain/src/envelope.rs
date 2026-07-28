//! Event and command envelopes — the two records every party agrees on.
//!
//! The envelope is the unit of append and of transport. Payloads above the
//! inline bound live outside the log as content-addressed blobs referenced by
//! [`PayloadRef`]; the inline `payload` field carries bounded JSON metadata only.
//!
//! Wire discipline (see `docs/contract/NAMING.md`): every field snake_case, all
//! optional fields omitted when absent, every 64-bit counter a decimal string.

use crate::ids::{
    AggregateId, ByteCount, CommandId, CorrelationId, EventId, IdempotencyKey, ProjectId, Seq,
    Timestamp,
};

/// The envelope schema version this crate reads and writes.
pub const ENVELOPE_SCHEMA_VERSION: u32 = 1;

/// Inline `payload` bound, in serialized bytes. Anything larger belongs in a
/// content-addressed blob behind [`PayloadRef`]. Enforced by the kernel at
/// append and re-checked by `gwk-cert`.
pub const INLINE_PAYLOAD_MAX_BYTES: usize = 64 * 1024;

/// The EXPORT-side shape of JSON metadata payloads. At runtime payloads are
/// `serde_json::Value` (exact round-trips, arbitrary integer precision); this
/// type exists so the generated TypeScript says "JSON tree", not `any` — and
/// it encodes the discipline that a payload NUMBER is IEEE-754 double
/// territory: anything 64-bit crosses the wire as a decimal string.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(untagged)]
pub enum JsonValue {
    Null(()),
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<JsonValue>),
    Object(std::collections::BTreeMap<String, JsonValue>),
}

/// Who caused an event or issued a command. `kind` is an OPEN bounded string
/// (e.g. `operator`, `kernel`, `engine`, `liveness_producer`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct Actor {
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub id: Option<String>,
}

/// The producing system, opaque to the contract (e.g. a kernel node name, an
/// adapter). `ref` optionally pins a system-local origin detail.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct Origin {
    pub system: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub r#ref: Option<String>,
}

/// Reference to an out-of-line, content-addressed payload blob.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct PayloadRef {
    /// Content address, `<scheme>:<hex>` (e.g. `sha256:…`). The scheme prefix is
    /// part of the address; consumers must not assume a single scheme forever.
    pub digest: String,
    pub media_type: String,
    pub byte_size: ByteCount,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub retention_class: Option<String>,
    /// Pinned as evidence: excluded from retention sweeps until unpinned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub evidence_pin: Option<bool>,
}

/// One appended event. Immutable once written; `global_sequence` is assigned by
/// the kernel append actor in commit order.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct EventEnvelope {
    pub event_id: EventId,
    pub project_id: ProjectId,
    /// OPEN bounded string naming the aggregate family (e.g. `task`, `attempt`).
    pub aggregate_type: String,
    pub aggregate_id: AggregateId,
    /// Per-aggregate version this event produced: contiguous from 1.
    pub aggregate_version: u32,
    /// OPEN bounded string naming the event (e.g. `task_state_changed`).
    pub event_type: String,
    pub schema_version: u32,
    pub global_sequence: Seq,
    /// When the change happened in the producing system.
    pub occurred_at: Timestamp,
    /// When the kernel append actor committed it (assigned, never client-set).
    pub appended_at: Timestamp,
    pub actor: Actor,
    pub origin: Origin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub causation_id: Option<EventId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub correlation_id: Option<CorrelationId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub idempotency_key: Option<IdempotencyKey>,
    /// Bounded inline JSON metadata (≤ [`INLINE_PAYLOAD_MAX_BYTES`] serialized).
    #[specta(type = JsonValue)]
    pub payload: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub payload_ref: Option<PayloadRef>,
}

/// One issued command. Commands are requests — only the events they cause are
/// truth. `idempotency_key` is required: reissuing the same key is a no-op.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct CommandEnvelope {
    pub command_id: CommandId,
    pub project_id: ProjectId,
    /// OPEN bounded string naming the command (e.g. `cancel_attempt`).
    pub command_type: String,
    pub schema_version: u32,
    pub issued_at: Timestamp,
    pub actor: Actor,
    pub origin: Origin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub target_aggregate_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub target_aggregate_id: Option<AggregateId>,
    /// CAS precondition: the target aggregate's current version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub expected_version: Option<u32>,
    pub idempotency_key: IdempotencyKey,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub causation_id: Option<EventId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub correlation_id: Option<CorrelationId>,
    #[specta(type = JsonValue)]
    pub payload: serde_json::Value,
}

/// A consumer met a `schema_version` it cannot read and has no upcaster for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnknownSchemaVersion {
    pub found: u32,
    pub supported: u32,
}

impl std::fmt::Display for UnknownSchemaVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unknown schema_version {} (supported: {}, no upcaster registered)",
            self.found, self.supported
        )
    }
}

impl std::error::Error for UnknownSchemaVersion {}

/// Accept `found` iff it is the supported version or an upcaster covers it.
/// Unknown versions are a typed error, never a silent best-effort read.
pub fn accept_schema_version(
    found: u32,
    supported: u32,
    upcast_from: &[u32],
) -> Result<(), UnknownSchemaVersion> {
    if found == supported || upcast_from.contains(&found) {
        Ok(())
    } else {
        Err(UnknownSchemaVersion { found, supported })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> EventEnvelope {
        EventEnvelope {
            event_id: EventId::new("evt-1"),
            project_id: ProjectId::new("proj-1"),
            aggregate_type: "task".into(),
            aggregate_id: AggregateId::new("task-1"),
            aggregate_version: 1,
            event_type: "task_state_changed".into(),
            schema_version: ENVELOPE_SCHEMA_VERSION,
            global_sequence: Seq::new(42),
            occurred_at: Timestamp::new("2026-07-27T00:00:00Z"),
            appended_at: Timestamp::new("2026-07-27T00:00:01Z"),
            actor: Actor {
                kind: "kernel".into(),
                id: None,
            },
            origin: Origin {
                system: "kernel".into(),
                r#ref: None,
            },
            causation_id: None,
            correlation_id: None,
            idempotency_key: None,
            payload: serde_json::json!({ "to": "working" }),
            payload_ref: None,
        }
    }

    #[test]
    fn envelope_round_trips_and_omits_absent_optionals() {
        let env = fixture();
        let json = serde_json::to_string(&env).expect("serialize");
        // Absent options are OMITTED, not null — the tri-state discipline.
        assert!(!json.contains("causation_id"));
        assert!(!json.contains("payload_ref"));
        // `r#ref` serializes as plain `ref`; Seq as a decimal string.
        assert!(json.contains("\"system\":\"kernel\""));
        assert!(json.contains("\"global_sequence\":\"42\""));
        let back: EventEnvelope = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, env);
    }

    #[test]
    fn envelope_rejects_unknown_fields() {
        let mut value = serde_json::to_value(fixture()).expect("to_value");
        value["surprise"] = serde_json::json!(true);
        assert!(serde_json::from_value::<EventEnvelope>(value).is_err());
    }

    #[test]
    fn schema_version_gate_is_typed() {
        assert!(accept_schema_version(1, 1, &[]).is_ok());
        assert!(accept_schema_version(0, 1, &[0]).is_ok());
        assert_eq!(
            accept_schema_version(9, 1, &[0]),
            Err(UnknownSchemaVersion {
                found: 9,
                supported: 1
            })
        );
    }
}
