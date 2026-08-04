//! Turning one adapter's already-normalized transcript output into the
//! `IngestRecord` command it warrants — and, when the record also marks a
//! dispatch-tree change, the `RegisterDispatchNode`/`TransitionDispatchNode`
//! command that goes with it.
//!
//! [`TranscriptRecord`] is deliberately not any adapter's own typed event
//! enum (`Message` in `gwk-adapter-claude`, `CodexEvent` in
//! `gwk-adapter-codex`, `Event` in `gwk-adapter-opencode`). Depending on
//! those crates from this one is not available in this change's execution
//! environment at all — see the crate root doc — but the layering is the
//! right call independent of that: origination should not need three
//! adapter-specific match arms and compile-time knowledge of every
//! adapter's internal shape. It consumes what an adapter already
//! normalized, re-expressed as JSON plus the one extra fact — a fan-out
//! hint — that a bare payload cannot carry on its own, so a fourth adapter
//! needs no change here at all.
//!
//! Derivation: none — command construction over an already-normalized JSON
//! value; no terminal byte is parsed and no engine process is supervised
//! here.

use gwk_domain::{
    AttemptId, DispatchNodeId, INLINE_PAYLOAD_MAX_BYTES, IdempotencyKey, IngestionKind,
    KernelCommand,
};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// One transcript record an adapter's normalized event stream already
/// produced, reduced to what origination needs from it.
///
/// `body` is the adapter's own normalized value, already `serde_json`
/// -encoded by whatever produced it — never a raw wire frame. Re-parsing a
/// wire frame here would be the adapter's job done a second time, in a
/// second place that could disagree with the first.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptRecord {
    pub kind: IngestionKind,
    pub body: Value,
    /// Present when this record also marks a dispatch-tree change — a
    /// subagent starting, a thread closing, a child session finishing.
    pub fan_out: Option<FanOutHint>,
}

/// A dispatch-tree change one transcript record carries, alongside its
/// ingestion body.
#[derive(Debug, Clone, PartialEq)]
pub enum FanOutHint {
    Started {
        dispatch_node_id: DispatchNodeId,
        parent_id: Option<DispatchNodeId>,
        attempt_id: Option<AttemptId>,
        kind: String,
        label: Option<String>,
    },
    Transitioned {
        dispatch_node_id: DispatchNodeId,
        to: String,
        expected_version: u32,
    },
}

/// Why a [`TranscriptRecord`] could not be originated.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum TranscriptOriginationError {
    #[error("a transcript record's ingestion body must be a JSON object, got {0}")]
    BodyNotAnObject(&'static str),
    #[error("a transcript record's ingestion body is empty")]
    BodyEmpty,
    #[error("ingestion body is {actual} bytes, over the {max}-byte inline bound")]
    BodyTooLarge { actual: usize, max: usize },
    #[error("a dispatch-node fan-out hint's {field} must not be empty")]
    FanOutFieldEmpty { field: &'static str },
}

/// One or two commands: always the `IngestRecord` the transcript body
/// warrants, plus the dispatch-node command a [`FanOutHint`] adds.
///
/// Order matters to a caller submitting both, in the order returned: the
/// dispatch-tree command goes FIRST when the hint is a start, so a
/// downstream reader can never observe an `IngestRecord` attributing
/// content to a node that is not yet registered — and LAST when the hint is
/// a transition, so the transcript explaining why a node finished is on the
/// log before the node itself is marked finished.
pub fn originate(
    record: TranscriptRecord,
) -> Result<Vec<KernelCommand>, TranscriptOriginationError> {
    let ingest = ingest_record(record.kind, &record.body)?;
    let Some(hint) = record.fan_out else {
        return Ok(vec![ingest]);
    };
    Ok(match dispatch_command(hint)? {
        DispatchCommand::Register(cmd) => vec![cmd, ingest],
        DispatchCommand::Transition(cmd) => vec![ingest, cmd],
    })
}

enum DispatchCommand {
    Register(KernelCommand),
    Transition(KernelCommand),
}

fn dispatch_command(hint: FanOutHint) -> Result<DispatchCommand, TranscriptOriginationError> {
    match hint {
        FanOutHint::Started {
            dispatch_node_id,
            parent_id,
            attempt_id,
            kind,
            label,
        } => {
            non_empty("dispatch_node_id", dispatch_node_id.as_str())?;
            non_empty("kind", &kind)?;
            Ok(DispatchCommand::Register(crate::dispatch_node::register(
                dispatch_node_id,
                parent_id,
                attempt_id,
                kind,
                label,
            )))
        }
        FanOutHint::Transitioned {
            dispatch_node_id,
            to,
            expected_version,
        } => {
            non_empty("dispatch_node_id", dispatch_node_id.as_str())?;
            non_empty("to", &to)?;
            Ok(DispatchCommand::Transition(
                crate::dispatch_node::transition(dispatch_node_id, to, expected_version),
            ))
        }
    }
}

fn non_empty(field: &'static str, value: &str) -> Result<(), TranscriptOriginationError> {
    if value.trim().is_empty() {
        return Err(TranscriptOriginationError::FanOutFieldEmpty { field });
    }
    Ok(())
}

/// Build the `IngestRecord` a normalized body warrants.
///
/// Every real adapter's normalized output serializes as a JSON object
/// (`Message`, `CodexEvent`, `Event` are all a struct or an enum-with-fields
/// under `serde_json`), so a body that is not one is the seeded
/// malformation this function refuses rather than passes through — see the
/// module's tests.
pub fn ingest_record(
    kind: IngestionKind,
    body: &Value,
) -> Result<KernelCommand, TranscriptOriginationError> {
    let object = body
        .as_object()
        .ok_or_else(|| TranscriptOriginationError::BodyNotAnObject(json_type_name(body)))?;
    if object.is_empty() {
        return Err(TranscriptOriginationError::BodyEmpty);
    }
    let size = serde_json::to_vec(body)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX);
    if size > INLINE_PAYLOAD_MAX_BYTES {
        return Err(TranscriptOriginationError::BodyTooLarge {
            actual: size,
            max: INLINE_PAYLOAD_MAX_BYTES,
        });
    }
    Ok(KernelCommand::IngestRecord {
        kind,
        payload: body.clone(),
        payload_ref: None,
    })
}

/// The idempotency key one `IngestRecord` mints: content-addressed, the same
/// convention `gw ingest submit` already uses
/// (`crates/gridwork/src/main.rs`'s `digest_of_json`) — identical transcript
/// content submitted twice (a host restart re-processing output from before
/// its last committed submission) lands once rather than twice.
pub fn ingest_key(kind: IngestionKind, body: &Value) -> IdempotencyKey {
    let serialized = serde_json::to_vec(body).unwrap_or_default();
    let digest: [u8; 32] = Sha256::digest(&serialized).into();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        hex.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    IdempotencyKey::new(format!("{}:{hex}", kind.as_str()))
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body() -> Value {
        serde_json::json!({"text": "hello"})
    }

    #[test]
    fn a_well_formed_record_with_no_fan_out_originates_one_ingest_record() {
        let record = TranscriptRecord {
            kind: IngestionKind::Session,
            body: body(),
            fan_out: None,
        };
        let commands = originate(record).expect("originates");
        assert_eq!(commands.len(), 1);
        assert!(matches!(
            &commands[0],
            KernelCommand::IngestRecord {
                kind: IngestionKind::Session,
                ..
            }
        ));
    }

    /// The verify command's named requirement: a seeded malformed transcript
    /// fails typed, never panics.
    #[test]
    fn a_malformed_transcript_record_fails_typed_not_a_panic() {
        // The seed: an ingestion body that is not a JSON object at all — a
        // bare string, which no real adapter's normalized output ever
        // produces (each one serializes a struct or an enum-with-fields,
        // both objects on the wire).
        let record = TranscriptRecord {
            kind: IngestionKind::Session,
            body: Value::String("not an object".to_owned()),
            fan_out: None,
        };
        let error = originate(record).expect_err("a non-object body must be refused, not accepted");
        assert_eq!(error, TranscriptOriginationError::BodyNotAnObject("string"));
    }

    #[test]
    fn an_empty_body_is_refused() {
        let record = TranscriptRecord {
            kind: IngestionKind::Session,
            body: serde_json::json!({}),
            fan_out: None,
        };
        assert_eq!(
            originate(record).expect_err("an empty body carries nothing to ingest"),
            TranscriptOriginationError::BodyEmpty
        );
    }

    #[test]
    fn an_oversized_body_is_refused_before_it_ever_reaches_the_kernel() {
        let huge = serde_json::json!({ "text": "x".repeat(INLINE_PAYLOAD_MAX_BYTES + 1) });
        let record = TranscriptRecord {
            kind: IngestionKind::Session,
            body: huge,
            fan_out: None,
        };
        assert!(matches!(
            originate(record),
            Err(TranscriptOriginationError::BodyTooLarge { .. })
        ));
    }

    #[test]
    fn a_started_hint_registers_before_the_ingest_record() {
        let record = TranscriptRecord {
            kind: IngestionKind::Session,
            body: body(),
            fan_out: Some(FanOutHint::Started {
                dispatch_node_id: DispatchNodeId::new("node-1"),
                parent_id: None,
                attempt_id: None,
                kind: "subagent".to_owned(),
                label: None,
            }),
        };
        let commands = originate(record).expect("originates");
        assert_eq!(commands.len(), 2);
        assert!(matches!(
            &commands[0],
            KernelCommand::RegisterDispatchNode { .. }
        ));
        assert!(matches!(&commands[1], KernelCommand::IngestRecord { .. }));
    }

    #[test]
    fn a_transitioned_hint_transitions_after_the_ingest_record() {
        let record = TranscriptRecord {
            kind: IngestionKind::Session,
            body: body(),
            fan_out: Some(FanOutHint::Transitioned {
                dispatch_node_id: DispatchNodeId::new("node-1"),
                to: "finished".to_owned(),
                expected_version: 2,
            }),
        };
        let commands = originate(record).expect("originates");
        assert_eq!(commands.len(), 2);
        assert!(matches!(&commands[0], KernelCommand::IngestRecord { .. }));
        assert!(matches!(
            &commands[1],
            KernelCommand::TransitionDispatchNode { .. }
        ));
    }

    #[test]
    fn an_empty_dispatch_node_id_in_a_hint_is_refused_typed() {
        let record = TranscriptRecord {
            kind: IngestionKind::Session,
            body: body(),
            fan_out: Some(FanOutHint::Started {
                dispatch_node_id: DispatchNodeId::new(""),
                parent_id: None,
                attempt_id: None,
                kind: "subagent".to_owned(),
                label: None,
            }),
        };
        assert_eq!(
            originate(record).expect_err("an empty node id must be refused"),
            TranscriptOriginationError::FanOutFieldEmpty {
                field: "dispatch_node_id"
            }
        );
    }

    #[test]
    fn identical_bodies_mint_the_same_ingest_key_and_different_bodies_do_not() {
        let one = ingest_key(IngestionKind::Session, &body());
        let two = ingest_key(IngestionKind::Session, &body());
        assert_eq!(one, two);
        let different = ingest_key(IngestionKind::Session, &serde_json::json!({"text": "bye"}));
        assert_ne!(one, different);
        // Two different kinds over the identical body must not collide
        // either — the kind is part of the key, not just the digest.
        let other_kind = ingest_key(IngestionKind::Cost, &body());
        assert_ne!(one, other_kind);
    }
}
