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
///   be created, so nothing could ever be allowed. The submit path instead
///   requires the logical actor kind `operator` for grant and revoke; the
///   kernel gates what a grant PERMITS, not the act of granting.
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
    // Decision 11's receipt, literally (6′ P16): a config edit leaves an
    // immutable row. The key is kind-refined — see `classification_key` for
    // why the general evidence verb is not the act gated here.
    ("record_evidence:config_change", "config_change"),
    // Raw terminal input is operator-authorized, or orchestrator-authorized
    // under a live grant. Every result is receipted by the submit path.
    (
        "send_pty_input",
        gwk_domain::command::PTY_INPUT_ACTION_CLASS,
    ),
];

const MATCHING_GRANT_BASIS: &str = "matching unexpired scoped grant";
const OPERATOR_BASIS: &str = "operator authority";
const UNGRANTED_BASIS: &str = "no matching unexpired scoped grant";
const ACTOR_KIND_BASIS: &str = "actor kind is neither operator nor orchestrator";

/// What the gate decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Not gated at all — no class, so no evaluation and no receipt.
    Unclassified,
    /// A matching unexpired grant covers this.
    Allow {
        action_class: &'static str,
        observed_basis: &'static str,
    },
    /// Gated, and nothing covers it. Raises attention; does not mutate.
    Page {
        action_class: &'static str,
        observed_basis: &'static str,
    },
}

impl Decision {
    pub(crate) fn action_class(&self) -> Option<&'static str> {
        match self {
            Self::Unclassified => None,
            Self::Allow { action_class, .. } | Self::Page { action_class, .. } => {
                Some(action_class)
            }
        }
    }

    pub(crate) fn observed_basis(&self) -> Option<&'static str> {
        match self {
            Self::Unclassified => None,
            Self::Allow { observed_basis, .. } | Self::Page { observed_basis, .. } => {
                Some(observed_basis)
            }
        }
    }
}

/// The key [`RISK_CLASS`] is consulted with for one command.
///
/// Almost always the command type itself. `record_evidence` is refined by its
/// `kind`: the verb is the lifecycle's general evidence carrier (`transcript`,
/// `diff`, `log`, …) and only the `config_change` kind is decision 11's
/// config-edit act. Classifying the whole verb would gate ordinary evidence
/// behind grants nobody has issued — and stamp a `config_change` receipt onto
/// a transcript record, a label that lies about the act it attests.
pub(crate) fn classification_key<'a>(
    command_type: &'a str,
    command: &gwk_domain::command::KernelCommand,
) -> &'a str {
    match command {
        gwk_domain::command::KernelCommand::RecordEvidence { kind, .. }
            if kind == "config_change" =>
        {
            "record_evidence:config_change"
        }
        _ => command_type,
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

    // PTY input has an explicit actor boundary: the operator is intrinsically
    // authorized on the same-EUID socket; only the orchestrator may cross it
    // under a grant. A grant accidentally issued to an engine is not enough.
    if action_class == gwk_domain::command::PTY_INPUT_ACTION_CLASS {
        if envelope.actor.kind == "operator" {
            return Ok(Decision::Allow {
                action_class,
                observed_basis: OPERATOR_BASIS,
            });
        }
        if envelope.actor.kind != "orchestrator" {
            return Ok(Decision::Page {
                action_class,
                observed_basis: ACTOR_KIND_BASIS,
            });
        }
    }

    let live_expiry = action_class == gwk_domain::command::PTY_INPUT_ACTION_CLASS;
    let allowed = matching_grant(
        conn,
        &envelope.actor,
        action_class,
        subject,
        envelope,
        live_expiry,
    )
    .await?;
    Ok(if allowed {
        Decision::Allow {
            action_class,
            observed_basis: MATCHING_GRANT_BASIS,
        }
    } else {
        Decision::Page {
            action_class,
            observed_basis: UNGRANTED_BASIS,
        }
    })
}

/// Is there a live grant for this actor, class, and subject?
///
/// Scope matches EXACTLY, and a grant with no scope covers its whole class.
/// No prefix and no pattern: the failure mode of a too-narrow grant is a
/// refusal a human then widens, while the failure mode of a too-broad one is a
/// command that should have paged and did not.
///
/// Most authority decisions compare expiry to the command's `issued_at`, which
/// preserves their historical replay semantics. PTY input is deliberately
/// stricter: it asks for a LIVE grant, so expiry is compared to the database
/// wall clock and cannot be bypassed by backdating a caller-controlled
/// envelope or by opening a transaction just before expiry and then waiting
/// on the writer lock. Idempotent replays do not re-enter authority evaluation.
async fn matching_grant(
    conn: &mut PgConnection,
    actor: &Actor,
    action_class: &str,
    subject: &str,
    envelope: &CommandEnvelope,
    live_expiry: bool,
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
           AND (expires_at IS NULL \
                 OR ($5 AND expires_at > clock_timestamp()) \
                OR (NOT $5 AND expires_at > $4::timestamptz)) \
         LIMIT 1",
    )
    .bind(action_class)
    .bind(
        serde_json::to_value(actor)
            .map_err(|e| Refusal::storage(format!("serialize actor: {e}")))?,
    )
    .bind(subject)
    .bind(envelope.issued_at.as_str())
    .bind(live_expiry)
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
        assert_eq!(class_of("send_pty_input"), Some("pty_input"));
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
                action_class: "stop",
                observed_basis: UNGRANTED_BASIS,
            }
            .action_class(),
            Some("stop")
        );
        assert_eq!(
            Decision::Allow {
                action_class: "pty_input",
                observed_basis: OPERATOR_BASIS,
            }
            .observed_basis(),
            Some(OPERATOR_BASIS)
        );
    }
}
