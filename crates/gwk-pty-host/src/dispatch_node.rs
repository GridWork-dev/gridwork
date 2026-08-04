//! Builders for the two dispatch-tree commands — `RegisterDispatchNode` and
//! `TransitionDispatchNode` — plus the idempotency keys origination mints
//! for them.
//!
//! Every other command family this host might originate already has a
//! builder living close to the fact it reports: `RecordCostEntry` in each
//! adapter's own `cost` module, `OpenGate` in `gwk-adapter-opencode`'s
//! `event` module. The dispatch tree does not, because no adapter crate
//! constructs a [`gwk_domain::DispatchNodeId`] or names a node's lifecycle
//! state — that vocabulary belongs to the host, the one component watching
//! every engine's fan-out at once. [`crate::ingest`] is where a normalized
//! transcript record reaches these.
//!
//! Derivation: none — command construction and key derivation only; no
//! terminal byte is parsed and no process is supervised here.

use gwk_domain::{AttemptId, DispatchNodeId, IdempotencyKey, KernelCommand};

/// Build the `RegisterDispatchNode` command body for a node the host just
/// observed starting.
pub fn register(
    dispatch_node_id: DispatchNodeId,
    parent_id: Option<DispatchNodeId>,
    attempt_id: Option<AttemptId>,
    kind: String,
    label: Option<String>,
) -> KernelCommand {
    KernelCommand::RegisterDispatchNode {
        dispatch_node_id,
        parent_id,
        attempt_id,
        kind,
        label,
    }
}

/// Build the `TransitionDispatchNode` command body for a node's lifecycle
/// change.
pub fn transition(
    dispatch_node_id: DispatchNodeId,
    to: String,
    expected_version: u32,
) -> KernelCommand {
    KernelCommand::TransitionDispatchNode {
        dispatch_node_id,
        to,
        expected_version,
    }
}

/// The idempotency key a retried `RegisterDispatchNode` for the same node
/// must present again.
///
/// A node is registered exactly once, so the node's own id is the whole
/// key — no other field of the command can vary between a genuine retry and
/// a second, different registration of the SAME node.
pub fn register_key(dispatch_node_id: &DispatchNodeId) -> IdempotencyKey {
    IdempotencyKey::new(format!("register_dispatch_node:{dispatch_node_id}"))
}

/// The idempotency key for one transition attempt.
///
/// `expected_version` is what makes this safe to key on alone: the
/// kernel's own CAS already requires it to advance between two REAL
/// transitions of the same node (`crates/gwk-kernel/src/submit.rs`), so two
/// calls carrying the same version are, by construction, retries of the
/// identical attempt — and two carrying different versions are different
/// transitions this key must not collide.
pub fn transition_key(dispatch_node_id: &DispatchNodeId, expected_version: u32) -> IdempotencyKey {
    IdempotencyKey::new(format!(
        "transition_dispatch_node:{dispatch_node_id}:{expected_version}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_builds_the_command_body_verbatim() {
        let command = register(
            DispatchNodeId::new("node-1"),
            Some(DispatchNodeId::new("node-0")),
            Some(AttemptId::new("att-1")),
            "subagent".to_owned(),
            Some("gw-rust-pro".to_owned()),
        );
        assert_eq!(command.command_type(), "register_dispatch_node");
        let KernelCommand::RegisterDispatchNode {
            dispatch_node_id,
            parent_id,
            attempt_id,
            kind,
            label,
        } = command
        else {
            panic!("expected RegisterDispatchNode, got {command:?}");
        };
        assert_eq!(dispatch_node_id, DispatchNodeId::new("node-1"));
        assert_eq!(parent_id, Some(DispatchNodeId::new("node-0")));
        assert_eq!(attempt_id, Some(AttemptId::new("att-1")));
        assert_eq!(kind, "subagent");
        assert_eq!(label, Some("gw-rust-pro".to_owned()));
    }

    #[test]
    fn transition_builds_the_command_body_verbatim() {
        let command = transition(DispatchNodeId::new("node-1"), "finished".to_owned(), 3);
        assert_eq!(command.command_type(), "transition_dispatch_node");
        let KernelCommand::TransitionDispatchNode {
            dispatch_node_id,
            to,
            expected_version,
        } = command
        else {
            panic!("expected TransitionDispatchNode, got {command:?}");
        };
        assert_eq!(dispatch_node_id, DispatchNodeId::new("node-1"));
        assert_eq!(to, "finished");
        assert_eq!(expected_version, 3);
    }

    #[test]
    fn a_retried_register_of_the_same_node_mints_the_same_key() {
        let id = DispatchNodeId::new("node-1");
        assert_eq!(register_key(&id), register_key(&id));
        assert_ne!(
            register_key(&id),
            register_key(&DispatchNodeId::new("node-2"))
        );
    }

    #[test]
    fn transitions_at_the_same_version_replay_and_different_versions_do_not_collide() {
        let id = DispatchNodeId::new("node-1");
        assert_eq!(transition_key(&id, 2), transition_key(&id, 2));
        assert_ne!(transition_key(&id, 2), transition_key(&id, 3));
    }
}
