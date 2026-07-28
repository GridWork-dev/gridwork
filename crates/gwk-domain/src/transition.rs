//! The one transition algorithm every writer goes through.
//!
//! `apply` is pure: given the aggregate's current cursor and a request, it
//! returns what happened as a value — [`TransitionResult`] — never a panic and
//! never a silent mutation. Storage layers persist an `applied` result
//! atomically with its receipt/outcome side rows; everything else is a typed
//! refusal the caller branches on.

use crate::envelope::Actor;
use crate::fsm::{AttemptState, CommandState, MessageState, StateMachine, TaskState};
use crate::ids::{IdempotencyKey, ReceiptId};

/// The actor `kind` of the ONE legal writer of the `running` ⇄ `blocked` flip
/// (D5: the liveness producer). Every flip also requires a receipt.
pub const LIVENESS_PRODUCER_KIND: &str = "liveness_producer";

/// An aggregate's current position: state, CAS version, and the idempotency
/// key of the last applied transition (what makes retries stable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor<S> {
    pub state: S,
    pub version: u32,
    pub applied_idempotency_key: Option<IdempotencyKey>,
}

/// One requested transition.
#[derive(Debug, Clone, Copy)]
pub struct TransitionRequest<'a, S> {
    pub to: S,
    /// CAS precondition: must equal the cursor's current version; the applied
    /// result carries `expected_version + 1`.
    pub expected_version: u32,
    pub actor: &'a Actor,
    pub idempotency_key: Option<&'a IdempotencyKey>,
    /// The receipt accompanying guarded edges (D5 blocked flips REQUIRE one).
    pub receipt_id: Option<&'a ReceiptId>,
}

/// What `apply` decided, as a tagged wire value.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TransitionResult<S> {
    Applied { state: S, version: u32 },
    IllegalEdge { from: S, to: S },
    StaleVersion { actual: u32, expected: u32 },
    UnauthorizedActor { reason: String },
}

/// Why a guard refused an otherwise-legal edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardViolation {
    WrongActor { required_kind: &'static str },
    MissingReceipt,
}

impl std::fmt::Display for GuardViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WrongActor { required_kind } => {
                write!(f, "edge requires actor kind {required_kind}")
            }
            Self::MissingReceipt => f.write_str("edge requires a receipt"),
        }
    }
}

/// What a guard sees about the request.
#[derive(Debug, Clone, Copy)]
pub struct GuardCtx<'a> {
    pub actor: &'a Actor,
    pub receipt_id: Option<&'a ReceiptId>,
}

/// Per-machine actor rules layered over edge legality. Default: any actor.
pub trait TransitionGuard: StateMachine {
    fn guard(_from: Self, _to: Self, _ctx: &GuardCtx<'_>) -> Result<(), GuardViolation> {
        Ok(())
    }
}

impl TransitionGuard for TaskState {}
impl TransitionGuard for MessageState {}
impl TransitionGuard for CommandState {}

impl TransitionGuard for AttemptState {
    /// D5: `running` ⇄ `blocked` has exactly one legal writer — the liveness
    /// producer — and every flip is receipted. All other edges are unguarded
    /// here (authority beyond the flip is the kernel's policy layer).
    fn guard(from: Self, to: Self, ctx: &GuardCtx<'_>) -> Result<(), GuardViolation> {
        let is_flip = matches!(
            (from, to),
            (Self::Running, Self::Blocked) | (Self::Blocked, Self::Running)
        );
        if is_flip {
            if ctx.actor.kind != LIVENESS_PRODUCER_KIND {
                return Err(GuardViolation::WrongActor {
                    required_kind: LIVENESS_PRODUCER_KIND,
                });
            }
            if ctx.receipt_id.is_none() {
                return Err(GuardViolation::MissingReceipt);
            }
        }
        Ok(())
    }
}

/// Decide one transition. Check order:
///
/// 1. **Idempotent retry** — a request whose key equals the last APPLIED key
///    AND whose target state the cursor already holds returns the current
///    cursor unchanged (stable even after the version advanced; the
///    transition already happened). A matching key requesting any OTHER
///    state falls through to its honest refusal — a replayed key never
///    converts a wrong request into an apply.
/// 2. **Edge legality** against [`StateMachine::EDGES`].
/// 3. **CAS version** — `expected_version` must equal the cursor's version.
/// 4. **Actor guard** ([`TransitionGuard::guard`], e.g. the D5 flip rule).
pub fn apply<S: TransitionGuard>(
    cursor: &Cursor<S>,
    req: &TransitionRequest<'_, S>,
) -> TransitionResult<S> {
    if let (Some(key), Some(applied)) =
        (req.idempotency_key, cursor.applied_idempotency_key.as_ref())
        && key == applied
        && cursor.state == req.to
    {
        return TransitionResult::Applied {
            state: cursor.state,
            version: cursor.version,
        };
    }
    if !S::can_transition(cursor.state, req.to) {
        return TransitionResult::IllegalEdge {
            from: cursor.state,
            to: req.to,
        };
    }
    if cursor.version != req.expected_version {
        return TransitionResult::StaleVersion {
            actual: cursor.version,
            expected: req.expected_version,
        };
    }
    let ctx = GuardCtx {
        actor: req.actor,
        receipt_id: req.receipt_id,
    };
    if let Err(violation) = S::guard(cursor.state, req.to, &ctx) {
        return TransitionResult::UnauthorizedActor {
            reason: violation.to_string(),
        };
    }
    TransitionResult::Applied {
        state: req.to,
        version: req.expected_version + 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kernel() -> Actor {
        Actor {
            kind: "kernel".into(),
            id: None,
        }
    }

    fn liveness() -> Actor {
        Actor {
            kind: LIVENESS_PRODUCER_KIND.into(),
            id: Some("lp-1".into()),
        }
    }

    fn cursor(state: AttemptState, version: u32) -> Cursor<AttemptState> {
        Cursor {
            state,
            version,
            applied_idempotency_key: None,
        }
    }

    #[test]
    fn applied_bumps_version_by_exactly_one() {
        let actor = kernel();
        let result = apply(
            &cursor(AttemptState::Queued, 3),
            &TransitionRequest {
                to: AttemptState::Leased,
                expected_version: 3,
                actor: &actor,
                idempotency_key: None,
                receipt_id: None,
            },
        );
        assert_eq!(
            result,
            TransitionResult::Applied {
                state: AttemptState::Leased,
                version: 4
            }
        );
    }

    #[test]
    fn illegal_edge_is_refused_before_version() {
        let actor = kernel();
        // Wrong edge AND wrong version: the edge refusal wins.
        let result = apply(
            &cursor(AttemptState::Queued, 3),
            &TransitionRequest {
                to: AttemptState::Succeeded,
                expected_version: 99,
                actor: &actor,
                idempotency_key: None,
                receipt_id: None,
            },
        );
        assert_eq!(
            result,
            TransitionResult::IllegalEdge {
                from: AttemptState::Queued,
                to: AttemptState::Succeeded
            }
        );
    }

    #[test]
    fn stale_version_reports_actual() {
        let actor = kernel();
        let result = apply(
            &cursor(AttemptState::Running, 7),
            &TransitionRequest {
                to: AttemptState::Succeeded,
                expected_version: 6,
                actor: &actor,
                idempotency_key: None,
                receipt_id: None,
            },
        );
        assert_eq!(
            result,
            TransitionResult::StaleVersion {
                actual: 7,
                expected: 6
            }
        );
    }

    #[test]
    fn blocked_flip_demands_the_liveness_producer() {
        let wrong = kernel();
        let result = apply(
            &cursor(AttemptState::Running, 2),
            &TransitionRequest {
                to: AttemptState::Blocked,
                expected_version: 2,
                actor: &wrong,
                idempotency_key: None,
                receipt_id: None,
            },
        );
        assert!(matches!(result, TransitionResult::UnauthorizedActor { .. }));
    }

    #[test]
    fn blocked_flip_demands_a_receipt() {
        let actor = liveness();
        let no_receipt = apply(
            &cursor(AttemptState::Blocked, 5),
            &TransitionRequest {
                to: AttemptState::Running,
                expected_version: 5,
                actor: &actor,
                idempotency_key: None,
                receipt_id: None,
            },
        );
        assert!(matches!(
            no_receipt,
            TransitionResult::UnauthorizedActor { .. }
        ));

        let receipt = ReceiptId::new("r-1");
        let with_receipt = apply(
            &cursor(AttemptState::Blocked, 5),
            &TransitionRequest {
                to: AttemptState::Running,
                expected_version: 5,
                actor: &actor,
                idempotency_key: None,
                receipt_id: Some(&receipt),
            },
        );
        assert_eq!(
            with_receipt,
            TransitionResult::Applied {
                state: AttemptState::Running,
                version: 6
            }
        );
    }

    #[test]
    fn blocked_terminal_paths_need_no_liveness_actor() {
        // blocked -> canceling/failed/unknown are escape edges, not flips.
        let actor = kernel();
        for to in [
            AttemptState::Canceling,
            AttemptState::Failed,
            AttemptState::Unknown,
        ] {
            let result = apply(
                &cursor(AttemptState::Blocked, 1),
                &TransitionRequest {
                    to,
                    expected_version: 1,
                    actor: &actor,
                    idempotency_key: None,
                    receipt_id: None,
                },
            );
            assert_eq!(
                result,
                TransitionResult::Applied {
                    state: to,
                    version: 2
                },
                "blocked -> {to:?} must not demand the liveness producer"
            );
        }
    }

    #[test]
    fn same_key_retry_is_stable_even_after_version_advanced() {
        let actor = kernel();
        let key = IdempotencyKey::new("once");
        let already_applied = Cursor {
            state: TaskState::Working,
            version: 2,
            applied_idempotency_key: Some(key.clone()),
        };
        // The caller retries with its original expected_version (1) — stale by
        // now — and the SAME key: stable Applied, no double-apply, no conflict.
        let result = apply(
            &already_applied,
            &TransitionRequest {
                to: TaskState::Working,
                expected_version: 1,
                actor: &actor,
                idempotency_key: Some(&key),
                receipt_id: None,
            },
        );
        assert_eq!(
            result,
            TransitionResult::Applied {
                state: TaskState::Working,
                version: 2
            }
        );

        // A DIFFERENT key gets the honest stale-version refusal.
        let other = IdempotencyKey::new("twice");
        let result = apply(
            &already_applied,
            &TransitionRequest {
                to: TaskState::Completed,
                expected_version: 1,
                actor: &actor,
                idempotency_key: Some(&other),
                receipt_id: None,
            },
        );
        assert_eq!(
            result,
            TransitionResult::StaleVersion {
                actual: 2,
                expected: 1
            }
        );
    }

    #[test]
    fn replayed_key_with_a_different_target_state_falls_through_to_refusal() {
        // A key replay only claims Applied when the cursor already holds the
        // requested state. The same key aimed anywhere else — here combined
        // with a wrong actor and a stale expected_version — must reach its
        // honest refusal, never Applied.
        let engine = Actor {
            kind: "engine".into(),
            id: Some("e-1".into()),
        };
        let key = IdempotencyKey::new("flip-once");
        let cur = Cursor {
            state: AttemptState::Running,
            version: 5,
            applied_idempotency_key: Some(key.clone()),
        };
        // Legal edge, stale version: the CAS refusal wins.
        let result = apply(
            &cur,
            &TransitionRequest {
                to: AttemptState::Succeeded,
                expected_version: 999,
                actor: &engine,
                idempotency_key: Some(&key),
                receipt_id: None,
            },
        );
        assert_eq!(
            result,
            TransitionResult::StaleVersion {
                actual: 5,
                expected: 999
            }
        );
        // An edge not in the table at all: the edge refusal wins.
        let result = apply(
            &cur,
            &TransitionRequest {
                to: AttemptState::Leased,
                expected_version: 999,
                actor: &engine,
                idempotency_key: Some(&key),
                receipt_id: None,
            },
        );
        assert_eq!(
            result,
            TransitionResult::IllegalEdge {
                from: AttemptState::Running,
                to: AttemptState::Leased
            }
        );
    }

    #[test]
    fn transition_result_wire_shape_is_tagged_snake_case() {
        let applied: TransitionResult<TaskState> = TransitionResult::Applied {
            state: TaskState::Working,
            version: 2,
        };
        assert_eq!(
            serde_json::to_value(&applied).expect("serialize"),
            serde_json::json!({ "kind": "applied", "state": "working", "version": 2 })
        );
        let refused: TransitionResult<TaskState> = TransitionResult::UnauthorizedActor {
            reason: "edge requires a receipt".into(),
        };
        assert_eq!(
            serde_json::to_value(&refused).expect("serialize"),
            serde_json::json!({ "kind": "unauthorized_actor", "reason": "edge requires a receipt" })
        );
    }
}
