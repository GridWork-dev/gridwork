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

/// The actor `kind` of the ONE legal writer of the `running` ⇄ `blocked`
/// flip — the liveness-producer flip rule. Every flip also requires a receipt.
pub const LIVENESS_PRODUCER_KIND: &str = "liveness_producer";

/// An aggregate's current position: state, CAS version, and the idempotency
/// key of the last applied transition (what makes retries stable).
///
/// This is an in-memory value passed to [`apply`], NOT a wire/bindings type
/// (no `serde`/`specta` derive), so it can carry the identity that recorded the
/// last applied transition without touching the generated contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor<S> {
    pub state: S,
    pub version: u32,
    pub applied_idempotency_key: Option<IdempotencyKey>,
    /// The actor that RECORDED the transition named by
    /// `applied_idempotency_key`. A keyed replay must present this same actor
    /// identity to receive the stable idempotent echo; a harvested key replayed
    /// by any other actor falls through to the honest refusal instead of being
    /// handed the current state+version. `None` when no attributed transition
    /// has been applied yet.
    pub applied_by: Option<Actor>,
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
    /// The receipt accompanying guarded edges (the blocked-flip rule REQUIRES one).
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
    /// The liveness-producer flip rule: `running` ⇄ `blocked` has exactly one
    /// legal writer — the liveness producer — and every flip is receipted.
    /// All other edges are unguarded here (authority beyond the flip is the
    /// kernel's policy layer).
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
/// 1. **Actor guard** ([`TransitionGuard::guard`], e.g. the liveness-producer
///    flip rule) — FIRST, before every other check, so a caller the guard
///    refuses gets the guard's refusal and learns nothing about the
///    aggregate's current state or version (`IllegalEdge` carries `from`,
///    `StaleVersion` carries `actual` — neither may reach an unauthorized
///    actor). Ahead of the idempotency short-circuit deliberately: a
///    replayed key from an unauthorized actor on a guarded edge must be
///    refused, not answered, or the short-circuit becomes a state probe. A
///    legitimate retry resends the same complete request (same actor, same
///    receipt), so it passes the guard again and stays stable. Unguarded
///    edges are untouched.
/// 2. **Idempotent retry** — a request whose key equals the last APPLIED key
///    AND whose target state the cursor already holds returns the current
///    cursor unchanged (stable even after the version advanced; the
///    transition already happened). A matching key requesting any OTHER
///    state falls through to its honest refusal — a replayed key never
///    converts a wrong request into an apply.
/// 3. **Edge legality** against [`StateMachine::EDGES`].
/// 4. **CAS version** — `expected_version` must equal the cursor's version.
pub fn apply<S: TransitionGuard>(
    cursor: &Cursor<S>,
    req: &TransitionRequest<'_, S>,
) -> TransitionResult<S> {
    let ctx = GuardCtx {
        actor: req.actor,
        receipt_id: req.receipt_id,
    };
    if let Err(violation) = S::guard(cursor.state, req.to, &ctx) {
        return TransitionResult::UnauthorizedActor {
            reason: violation.to_string(),
        };
    }
    if let (Some(key), Some(applied)) =
        (req.idempotency_key, cursor.applied_idempotency_key.as_ref())
        && key == applied
        && cursor.state == req.to
        && cursor.applied_by.as_ref() == Some(req.actor)
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
    // A transition at the u32 version ceiling is a refusal, not a panic (debug)
    // or a silent wrap to 0 (release, a version REWIND — the other thing this
    // module forbids). Reuse the StaleVersion refusal the caller already knows.
    let Some(next_version) = req.expected_version.checked_add(1) else {
        return TransitionResult::StaleVersion {
            actual: cursor.version,
            expected: req.expected_version,
        };
    };
    TransitionResult::Applied {
        state: req.to,
        version: next_version,
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
            applied_by: None,
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
        // Pins the refusal order guard -> idempotency -> edge -> CAS on an
        // UNGUARDED edge: wrong edge AND wrong version, the edge refusal wins.
        // (Guard precedence has its own pin below.)
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
    fn version_ceiling_is_refused_not_overflowed() {
        // At version u32::MAX the "+1" would panic in debug and silently wrap
        // to 0 (a version rewind) in release. A legal, CAS-matching request at
        // the ceiling must instead get the honest StaleVersion refusal.
        let actor = kernel();
        let result = apply(
            &Cursor {
                state: TaskState::Submitted,
                version: u32::MAX,
                applied_idempotency_key: None,
                applied_by: None,
            },
            &TransitionRequest {
                to: TaskState::Working,
                expected_version: u32::MAX,
                actor: &actor,
                idempotency_key: None,
                receipt_id: None,
            },
        );
        assert_eq!(
            result,
            TransitionResult::StaleVersion {
                actual: u32::MAX,
                expected: u32::MAX,
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
            applied_by: Some(kernel()),
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
    fn guarded_flip_refuses_the_wrong_actor_before_disclosing_the_version() {
        // A wrong-actor request on the running -> blocked flip, carrying a
        // harvested idempotency key AND a stale expected_version, gets the
        // actor refusal — never StaleVersion { actual } (a version probe) and
        // never a keyed Applied echo.
        let engine = Actor {
            kind: "engine".into(),
            id: Some("e-1".into()),
        };
        let key = IdempotencyKey::new("flip-once");
        let receipt = ReceiptId::new("r-forged");
        let result = apply(
            &Cursor {
                state: AttemptState::Running,
                version: 5,
                applied_idempotency_key: Some(key.clone()),
                applied_by: None,
            },
            &TransitionRequest {
                to: AttemptState::Blocked,
                expected_version: 999,
                actor: &engine,
                idempotency_key: Some(&key),
                receipt_id: Some(&receipt),
            },
        );
        assert!(matches!(result, TransitionResult::UnauthorizedActor { .. }));
    }

    #[test]
    fn keyed_replay_requires_the_recording_actor_not_just_the_key() {
        // After a guarded flip that the liveness producer applied, the cursor
        // remembers WHO applied it. The (blocked, blocked) self-edge is not a
        // flip, so the guard does not fire on a replay to the current state —
        // the idempotency arm decides. Binding that arm to the recording actor
        // is what stops a harvested key from becoming a state+version probe.
        let lp = liveness();
        let key = IdempotencyKey::new("flip-once");
        let cur = Cursor {
            state: AttemptState::Blocked,
            version: 6,
            applied_idempotency_key: Some(key.clone()),
            applied_by: Some(lp.clone()),
        };

        // The recording actor replaying the key on the current state still gets
        // the stable idempotent echo — retries stay stable.
        let stable = apply(
            &cur,
            &TransitionRequest {
                to: AttemptState::Blocked,
                expected_version: 6,
                actor: &lp,
                idempotency_key: Some(&key),
                receipt_id: None,
            },
        );
        assert_eq!(
            stable,
            TransitionResult::Applied {
                state: AttemptState::Blocked,
                version: 6
            }
        );

        // A DIFFERENT actor replaying the harvested key falls through to the
        // honest refusal — never a keyed Applied { .., 6 } echo. The version
        // stays undisclosed; the residual `from` in IllegalEdge is the
        // CAS-retry contract's acknowledged, world-readable channel.
        let engine = Actor {
            kind: "engine".into(),
            id: Some("e-1".into()),
        };
        let probe = apply(
            &cur,
            &TransitionRequest {
                to: AttemptState::Blocked,
                expected_version: 6,
                actor: &engine,
                idempotency_key: Some(&key),
                receipt_id: None,
            },
        );
        assert_eq!(
            probe,
            TransitionResult::IllegalEdge {
                from: AttemptState::Blocked,
                to: AttemptState::Blocked
            }
        );
    }

    /// A test-only machine whose guard covers EVERY requested transition, so
    /// the guard's precedence over the idempotency short-circuit, the edge
    /// check, and the CAS check is observable in one request. (The real
    /// attempt flip pairs are all legal edges, so they cannot exercise the
    /// guard-vs-IllegalEdge ordering.)
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Gated {
        Shut,
    }

    impl StateMachine for Gated {
        const STATES: &'static [Self] = &[Self::Shut];
        const EDGES: &'static [(Self, Self)] = &[];
        const ESCAPE: &'static [Self] = &[];
    }

    impl TransitionGuard for Gated {
        fn guard(_from: Self, _to: Self, ctx: &GuardCtx<'_>) -> Result<(), GuardViolation> {
            if ctx.actor.kind != "gatekeeper" {
                return Err(GuardViolation::WrongActor {
                    required_kind: "gatekeeper",
                });
            }
            Ok(())
        }
    }

    #[test]
    fn guard_refusal_precedes_idempotency_edge_and_version() {
        // Everything else would fire: the key matches the last applied key
        // AND the cursor holds the requested state (idempotency would echo
        // Applied), the edge is not in the table (IllegalEdge would disclose
        // `from`), and the version is stale (StaleVersion would disclose
        // `actual`). The guard answers first; the caller learns nothing.
        let intruder = kernel();
        let key = IdempotencyKey::new("harvested");
        let result = apply(
            &Cursor {
                state: Gated::Shut,
                version: 4,
                applied_idempotency_key: Some(key.clone()),
                applied_by: None,
            },
            &TransitionRequest {
                to: Gated::Shut,
                expected_version: 999,
                actor: &intruder,
                idempotency_key: Some(&key),
                receipt_id: None,
            },
        );
        assert!(matches!(result, TransitionResult::UnauthorizedActor { .. }));
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
            applied_by: None,
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
