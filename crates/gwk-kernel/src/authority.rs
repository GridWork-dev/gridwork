//! Authority evaluation: which commands are gated, and what the gate answers.
//!
//! The contract's precedence, in order:
//!
//! 1. an invariant or explicit prohibition → **deny**;
//! 2. absent or risk-gated authority → **page**;
//! 3. a matching unexpired scoped grant → **allow**.
//!
//! Every result writes an immutable receipt in the same transaction. A page
//! also raises a deduplicated unresolved attention item and does NOT mutate the
//! target — it is a refusal that leaves a trail, not a queued write.

use gwk_domain::envelope::{Actor, CommandEnvelope};
use sqlx::PgConnection;

use crate::project::Refusal;

/// The commands an operator grant gates, and the class each falls in.
///
/// A command absent from this table is UNCLASSIFIED: it is not evaluated, gets
/// no receipt, and behaves exactly as it did before authority existed. That is
/// the honest default — `action_class` is an open string and no accepted
/// artifact maps commands onto it, so inventing a class for the whole work
/// lifecycle would gate ordinary work behind grants nobody has issued.
///
/// Two commands that look like obvious candidates are deliberately absent, and
/// the reason is the same for both: a gate that cannot be opened is a deadlock,
/// not a gate.
///
/// - `grant_authority` — gating it means the first grant needs a grant. No
///   grant exists on a fresh kernel, so it would page, so no grant could ever
///   be created, so nothing could ever be allowed. Granting is an operator act
///   arriving over a same-EUID socket; the kernel gates what a grant PERMITS,
///   not the act of granting.
/// - `activate_kernel` — genesis runs before any grant can exist, for the same
///   reason. Activation is protected structurally instead (exactly one genesis
///   append against a sealed allowlist), which is a stronger guarantee than a
///   grant lookup and does not depend on prior state.
///
/// Adding a class later is one line here plus its test. Removing the bootstrap
/// exclusions is not — read the two paragraphs above first.
const RISK_CLASS: &[(&str, &str)] = &[
    // The stop/kill spine. Killing running work is the destructive act in a
    // system whose whole job is to keep work running.
    ("issue_command", "stop"),
];

/// What the gate decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Not gated at all — no class, so no evaluation and no receipt.
    Unclassified,
    /// A matching unexpired grant covers this.
    Allow { action_class: &'static str },
    /// Gated, and nothing covers it. Raises attention; does not mutate.
    Page { action_class: &'static str },
}

impl Decision {
    pub(crate) fn action_class(&self) -> Option<&'static str> {
        match self {
            Self::Unclassified => None,
            Self::Allow { action_class } | Self::Page { action_class } => Some(action_class),
        }
    }
}

/// The class this command falls in, if any.
pub(crate) fn class_of(command_type: &str) -> Option<&'static str> {
    RISK_CLASS
        .iter()
        .find(|(name, _)| *name == command_type)
        .map(|(_, class)| *class)
}

/// Evaluate one command against the grants on record.
///
/// Read under the writer lock the caller already holds, so a grant revoked in a
/// concurrent transaction cannot be the one that allows this.
pub(crate) async fn evaluate(
    conn: &mut PgConnection,
    envelope: &CommandEnvelope,
    command_type: &str,
    subject: &str,
) -> Result<Decision, Refusal> {
    let Some(action_class) = class_of(command_type) else {
        return Ok(Decision::Unclassified);
    };
    let allowed = matching_grant(conn, &envelope.actor, action_class, subject, envelope).await?;
    Ok(if allowed {
        Decision::Allow { action_class }
    } else {
        Decision::Page { action_class }
    })
}

/// Is there a live grant for this actor, class, and subject?
///
/// Scope matches EXACTLY, and a grant with no scope covers its whole class.
/// No prefix and no pattern: the failure mode of a too-narrow grant is a
/// refusal a human then widens, while the failure mode of a too-broad one is a
/// command that should have paged and did not.
///
/// "Unexpired" is measured against the command's own `issued_at` rather than
/// the database clock, so replaying the log reaches the same verdict it did
/// live — a grant that had expired by wall-clock replay time must not turn a
/// historical allow into a page.
async fn matching_grant(
    conn: &mut PgConnection,
    actor: &Actor,
    action_class: &str,
    subject: &str,
    envelope: &CommandEnvelope,
) -> Result<bool, Refusal> {
    // The grant's id, not `SELECT 1`: a bare literal comes back INT4 while the
    // obvious Rust type for it is i64, and the driver refuses that decode
    // outright. Selecting the id sidesteps the width question and names the
    // grant that allowed this if anyone ever needs to log it.
    let found: Option<String> = sqlx::query_scalar(
        "SELECT id FROM gwk.authority_grant \
         WHERE action_class = $1 \
           AND grantee = $2::jsonb \
           AND (scope IS NULL OR scope = $3) \
           AND revoked_at IS NULL \
           AND (expires_at IS NULL OR expires_at > $4::timestamptz) \
         LIMIT 1",
    )
    .bind(action_class)
    .bind(
        serde_json::to_value(actor)
            .map_err(|e| Refusal::storage(format!("serialize actor: {e}")))?,
    )
    .bind(subject)
    .bind(envelope.issued_at.as_str())
    .fetch_optional(conn)
    .await
    .map_err(|e| Refusal::storage(format!("read authority grants: {e}")))?;
    Ok(found.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_named_commands_are_gated() {
        assert_eq!(class_of("issue_command"), Some("stop"));
        assert_eq!(class_of("create_task"), None);
        assert_eq!(class_of("transition_attempt"), None);
    }

    #[test]
    fn the_bootstrap_commands_are_not_gated() {
        // Both would deadlock: granting would need a grant, and genesis runs
        // before any grant can exist. If either of these ever returns Some,
        // a fresh kernel can no longer be brought up.
        assert_eq!(class_of("grant_authority"), None);
        assert_eq!(class_of("revoke_authority"), None);
        assert_eq!(class_of("activate_kernel"), None);
    }

    #[test]
    fn an_unclassified_decision_carries_no_class() {
        assert_eq!(Decision::Unclassified.action_class(), None);
        assert_eq!(
            Decision::Page {
                action_class: "stop"
            }
            .action_class(),
            Some("stop")
        );
    }
}
