//! Minting the [`CommandEnvelope`] around one command this host originated.
//!
//! Derivation: none — envelope construction only; no terminal byte is
//! parsed and no process is supervised here.

use gwk_domain::{
    Actor, CommandEnvelope, CommandId, ENVELOPE_SCHEMA_VERSION, IdempotencyKey, KernelCommand,
    Origin, ProjectId, Timestamp,
};

/// This host's `origin.system` label on every envelope it mints.
pub const ORIGIN_SYSTEM: &str = "gwk-pty-host";

/// This host's `actor.kind` label on every envelope it mints — distinct
/// from `"operator"` (`gw`'s own CLI, `crates/gridwork/src/lib.rs`) and
/// from `"kernel"` (the log's own actor on events it produces itself,
/// `crates/gwk-kernel/src/project.rs`): a command this host originates is
/// neither a human's request nor the kernel's own bookkeeping — it is what
/// a supervised engine said, relayed by the process supervising it.
pub const ACTOR_KIND: &str = "host";

/// Build the envelope for `command`, addressed by `key`.
///
/// `command_id` is DERIVED from the idempotency key, not randomly minted —
/// the same convention `gridwork`'s own CLI already uses for the commands
/// it mints itself (`crates/gridwork/src/lib.rs`'s `envelope()`: `CommandId
/// ::new(format!("cmd-{key}"))`), so the two surfaces in this workspace that
/// mint their own envelopes agree on one scheme rather than each inventing
/// its own. That is what makes a retried origination — a reconnect after a
/// dropped connection, a restart re-processing a transcript record it
/// cannot yet know it already originated — land as the SAME command rather
/// than a second one: both the id and the key are a pure function of `key`
/// alone, never of the clock or of process state.
///
/// `target_aggregate_type`/`target_aggregate_id`/`expected_version` stay
/// `None` at the envelope level, matching that same precedent: the kernel's
/// own submit path decides CAS from the command BODY's own
/// `expected_version` field where one exists
/// (`crates/gwk-kernel/src/submit.rs`), never from the envelope's.
pub fn mint(
    command: &KernelCommand,
    key: IdempotencyKey,
    project: ProjectId,
    issued_at: Timestamp,
) -> CommandEnvelope {
    CommandEnvelope {
        command_id: CommandId::new(format!("cmd-{key}")),
        project_id: project,
        command_type: command.command_type().to_owned(),
        schema_version: ENVELOPE_SCHEMA_VERSION,
        issued_at,
        actor: Actor {
            kind: ACTOR_KIND.to_owned(),
            id: None,
        },
        origin: Origin {
            system: ORIGIN_SYSTEM.to_owned(),
            r#ref: None,
        },
        target_aggregate_type: None,
        target_aggregate_id: None,
        expected_version: None,
        idempotency_key: key,
        causation_id: None,
        correlation_id: None,
        // Infallible for a command built by this crate — every `KernelCommand`
        // variant serializes — and a null payload would be refused by the
        // kernel anyway, the same reasoning `gridwork`'s own `envelope()`
        // states for the identical fallback.
        payload: serde_json::to_value(command).unwrap_or(serde_json::Value::Null),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gwk_domain::{AttentionItemId, IngestionKind};

    fn ts() -> Timestamp {
        Timestamp::new("2026-08-01T00:00:00Z")
    }

    fn ingest_command() -> KernelCommand {
        KernelCommand::IngestRecord {
            kind: IngestionKind::Session,
            payload: serde_json::json!({"text": "hi"}),
            payload_ref: None,
        }
    }

    #[test]
    fn the_command_id_is_a_pure_function_of_the_key_so_a_retry_lands_once() {
        let command = ingest_command();
        let one = mint(
            &command,
            IdempotencyKey::new("k-1"),
            ProjectId::new("proj"),
            ts(),
        );
        let two = mint(
            &command,
            IdempotencyKey::new("k-1"),
            ProjectId::new("proj"),
            ts(),
        );
        assert_eq!(one.command_id, two.command_id);
        assert_eq!(one.idempotency_key, two.idempotency_key);
        assert_eq!(one.payload, two.payload);
        assert_eq!(one.command_type, "ingest_record");
    }

    #[test]
    fn two_different_keys_mint_two_different_command_ids() {
        let command = ingest_command();
        let one = mint(
            &command,
            IdempotencyKey::new("k-1"),
            ProjectId::new("proj"),
            ts(),
        );
        let two = mint(
            &command,
            IdempotencyKey::new("k-2"),
            ProjectId::new("proj"),
            ts(),
        );
        assert_ne!(one.command_id, two.command_id);
    }

    #[test]
    fn the_actor_and_origin_identify_this_host_not_the_operator_or_the_kernel() {
        let command = KernelCommand::ResolveAttention {
            attention_item_id: AttentionItemId::new("a-1"),
            resolution: None,
        };
        let envelope = mint(
            &command,
            IdempotencyKey::new("k"),
            ProjectId::new("proj"),
            ts(),
        );
        assert_eq!(envelope.actor.kind, ACTOR_KIND);
        assert_eq!(envelope.origin.system, ORIGIN_SYSTEM);
        // Envelope-level CAS fields stay unset — the body's own field is
        // what the kernel's submit path actually reads.
        assert_eq!(envelope.target_aggregate_type, None);
        assert_eq!(envelope.target_aggregate_id, None);
        assert_eq!(envelope.expected_version, None);
    }
}
