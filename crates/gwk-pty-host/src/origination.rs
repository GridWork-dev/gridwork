//! Tying origination together: mint an envelope for a command this host
//! built, submit it, and classify what the kernel answered into something a
//! caller can act on without re-deriving `KernelErrorCode` semantics at
//! every call site.
//!
//! Derivation: none — result classification and submission plumbing only;
//! no terminal byte or process behavior is asserted here.

use gwk_domain::{
    EventEnvelope, IdempotencyKey, KernelCommand, KernelErrorCode, KernelResult, ProjectId, Seq,
    Timestamp,
};

use crate::envelope;
use crate::kernel_client::{KernelClient, KernelClientError};

/// What submitting one command settled, once the wire-level [`KernelResult`]
/// is reduced to what a caller of this host actually needs to decide.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Applied — freshly, or as an idempotent replay of an identical prior
    /// submission. The kernel does not distinguish the two on the wire (its
    /// own doc on `KernelResult::CommandApplied`: "an idempotent replay
    /// answers with the original [events]"), and neither does this: both
    /// mean the command landed.
    Applied {
        events: Vec<EventEnvelope>,
        watermark: Seq,
    },
    /// A version-bearing command (a transition) was refused because the
    /// aggregate has moved past `expected_version`. Retryable ONLY by a
    /// caller that re-reads the current version and re-originates — this
    /// type never retries on its own, because guessing the next version
    /// would be racing whoever else moved it.
    StaleVersion {
        code: KernelErrorCode,
        message: String,
    },
    /// Any other refusal. Not retryable by resubmitting the identical
    /// envelope unchanged — the key is either permanently wrong for what it
    /// names, or the refusal is about something other than timing.
    Refused {
        code: KernelErrorCode,
        message: String,
    },
}

/// Reduce a raw [`KernelResult`] to an [`Outcome`].
///
/// `submit`'s own contract (`crates/gwk-kernel/src/submit.rs`'s
/// `route_of`, matched exhaustively with no wildcard arm) answers a
/// `SubmitCommand` request with exactly `CommandApplied` or `Error` —
/// never a projection, a blob, or a subscription result. The catch-all arm
/// below exists for that guarantee living in a DIFFERENT crate, not for an
/// expected case: it turns "this cannot happen today" into a reported
/// refusal instead of a panic if a future protocol addition ever proved it
/// wrong.
fn classify(result: KernelResult) -> Outcome {
    match result {
        KernelResult::CommandApplied {
            events, watermark, ..
        } => Outcome::Applied { events, watermark },
        KernelResult::Error { code, message, .. } if code == KernelErrorCode::StaleVersion => {
            Outcome::StaleVersion { code, message }
        }
        KernelResult::Error { code, message, .. } => Outcome::Refused { code, message },
        other => Outcome::Refused {
            code: KernelErrorCode::Schema,
            message: format!("SubmitCommand answered with an unexpected result: {other:?}"),
        },
    }
}

/// Mint, submit, and classify one command this host originated.
pub async fn submit(
    client: &mut KernelClient,
    command: &KernelCommand,
    key: IdempotencyKey,
    project: ProjectId,
    issued_at: Timestamp,
) -> Result<Outcome, KernelClientError> {
    let envelope = envelope::mint(command, key, project, issued_at);
    let result = client.submit(envelope).await?;
    Ok(classify(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gwk_domain::CommandId;

    #[test]
    fn an_applied_result_classifies_as_applied() {
        let outcome = classify(KernelResult::CommandApplied {
            command_id: CommandId::new("cmd-1"),
            events: Vec::new(),
            watermark: Seq::new(9),
        });
        assert!(matches!(
            outcome,
            Outcome::Applied { watermark, .. } if watermark == Seq::new(9)
        ));
    }

    #[test]
    fn a_stale_version_refusal_classifies_separately_from_every_other_refusal() {
        let stale = classify(KernelResult::Error {
            code: KernelErrorCode::StaleVersion,
            message: "old".to_owned(),
            detail: None,
        });
        assert!(matches!(stale, Outcome::StaleVersion { .. }));

        let other = classify(KernelResult::Error {
            code: KernelErrorCode::Authority,
            message: "no".to_owned(),
            detail: None,
        });
        assert!(matches!(
            other,
            Outcome::Refused {
                code: KernelErrorCode::Authority,
                ..
            }
        ));
    }

    #[test]
    fn a_result_no_submit_command_answer_can_ever_be_classifies_as_a_refusal_not_a_panic() {
        let outcome = classify(KernelResult::Watermark { watermark: None });
        assert!(matches!(
            outcome,
            Outcome::Refused {
                code: KernelErrorCode::Schema,
                ..
            }
        ));
    }
}
