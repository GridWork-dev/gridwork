//! [`EngineAdapter`] for codex: the app-server's vocabulary, reduced to the
//! four facts `docs/PARITY.md` asks every engine for.
//!
//! [`CodexEvent`] stays the crate's own richer surface — it carries turn ids,
//! typed items, retry intent, and token usage that a caller driving codex
//! specifically needs. This module is the lossy half on purpose: what survives
//! is what a supervisor can ask of all three engines without knowing which one
//! it has.
//!
//! # The approval gap, stated rather than papered over
//!
//! [`EngineEvent::ApprovalAsked`] is unreachable from here. Codex delivers an
//! approval as a server-initiated JSON-RPC *request*, which this crate models as
//! [`crate::ApprovalRequest`] through `normalize_request` — a different type on a
//! different channel from the [`CodexEvent`] notifications this `Raw` covers.
//!
//! The sibling claude adapter met the same split and solved it with a union
//! `Raw` over its two channels. Doing that here is the right fix and is not
//! done in this change: it would widen `Raw` to a type nothing constructs yet,
//! and an approval that no consumer reads is worth less than an honest gap. So
//! codex reports three of the four axes through this trait, and axis 4 stays on
//! `normalize_request` where its consumers already are.
//!
//! # Clean-room scope
//!
//! No new protocol behavior is read here. Every mapping below is over
//! [`CodexEvent`], which this crate already derived and cited; the derivations
//! for `thread/started`, `thread/status/changed`, `turn/completed` and the rest
//! sit at their parse sites in [`crate::event`] and [`crate::schema`]. What
//! this module decides is which of `docs/PARITY.md`'s axis entries each already-
//! parsed value belongs to, and that document is this repository's own — see the
//! declaration below the doc block.

// Derivation: none — this module reads no protocol. Every value it matches on is
// already parsed and already cited at its parse site in a sibling module, and
// what it decides is which entry of `docs/PARITY.md`'s own axis tables each one
// belongs to. That is a mapping onto this repository's contract, not a reading
// of a vendor's, so there is no source to name — and naming one would put a
// citation on a decision the cited page never made.

use gwk_domain::engine::{EngineAdapter, EngineEvent, EngineStatus, LifecycleFact};
use gwk_domain::ids::{EngineId, EngineSessionId};

use crate::event::CodexEvent;
use crate::schema::ThreadStatus;

/// The codex adapter's normalization half.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CodexAdapter;

/// A codex thread id is the engine session id, carried in the engine's own
/// spelling. Translating it would strand a caller that has to name the thread
/// back to the app-server.
fn session(thread_id: &str) -> EngineSessionId {
    EngineSessionId::new(thread_id)
}

/// The three states `docs/PARITY.md` axis 2 allows, from codex's four.
///
/// `notLoaded` and `systemError` collapse to idle because axis 2 offers no
/// other home: its three states are working, idle, and waiting on approval, and
/// neither of these says the engine is working. That is a lossy fit and not a
/// claim that they MEAN idle — `CodexEvent` still carries the exact
/// `ThreadStatus` for a caller that needs the difference, and only
/// `ThreadStatus::Idle` is treated as the contract's idle LIFECYCLE surface, so
/// nothing downstream reads a system error as a turn finishing.
fn status_of(status: &ThreadStatus) -> EngineStatus {
    if status.is_waiting_on_approval() {
        return EngineStatus::WaitingOnApproval;
    }
    match status {
        ThreadStatus::Active { .. } => EngineStatus::Working,
        ThreadStatus::Idle | ThreadStatus::NotLoaded | ThreadStatus::SystemError => {
            EngineStatus::Idle
        }
    }
}

impl EngineAdapter for CodexAdapter {
    type Raw = CodexEvent;
    /// Infallible: [`CodexEvent`] is already the parsed, validated form, and
    /// every parse failure this crate can have was raised as
    /// [`crate::NormalizeError`] before one existed. A `Result` here would be a
    /// second error channel that can never carry a value.
    type Error = std::convert::Infallible;

    fn engine_id(&self) -> EngineId {
        crate::engine_id()
    }

    fn normalize(&self, raw: CodexEvent) -> Result<Vec<EngineEvent>, Self::Error> {
        Ok(match raw {
            CodexEvent::ThreadStarted { thread_id, status } => vec![
                EngineEvent::Lifecycle {
                    session: session(&thread_id),
                    fact: LifecycleFact::Started,
                },
                // The start notification carries a status, and dropping it
                // would make the first status push look like the first status.
                EngineEvent::Status {
                    session: session(&thread_id),
                    status: status_of(&status),
                },
            ],
            // `docs/PARITY.md` axis 1's codex row gives the idle fact as
            // `thread/status/changed` -> `idle`, so this arm carries BOTH: the
            // status push, and — when that status is idle — the lifecycle fact
            // the contract says this surface is where you observe.
            CodexEvent::StatusChanged { thread_id, status } => {
                let mut events = vec![EngineEvent::Status {
                    session: session(&thread_id),
                    status: status_of(&status),
                }];
                if matches!(status, ThreadStatus::Idle) {
                    events.push(EngineEvent::Lifecycle {
                        session: session(&thread_id),
                        fact: LifecycleFact::Idle,
                    });
                }
                events
            }
            CodexEvent::ThreadClosed { thread_id } => vec![EngineEvent::Lifecycle {
                session: session(&thread_id),
                fact: LifecycleFact::Ended,
            }],
            // `docs/PARITY.md` axis 1's codex row lists `turn/completed` in the
            // ERROR column and nowhere else — "`turn/completed` carrying
            // `error`, or the `error` notification". So the only lifecycle fact
            // this notification can carry is an error, and only when it
            // actually carries one. A turn ending is a turn boundary; the
            // contract assigns idle to `thread/status/changed` and end to
            // `thread/closed`, both of which arrive separately.
            //
            // `turn.error` is the condition rather than the status, because
            // `error` is the thing the row names and the vendored schema scopes
            // it exactly: populated only when the turn failed. Reading
            // `interrupted` as an error would be this file's inference, not the
            // row's claim — an interrupted turn is one somebody stopped.
            CodexEvent::TurnCompleted { thread_id, turn } => {
                if turn.error.is_some() {
                    vec![EngineEvent::Lifecycle {
                        session: session(&thread_id),
                        fact: LifecycleFact::Errored,
                    }]
                } else {
                    Vec::new()
                }
            }
            // `will_retry` is deliberately not consulted. A retry that has not
            // happened does not un-happen the error, and a supervisor that
            // waited for the retry before believing the error would be blind
            // for exactly as long as the engine takes to fail again.
            CodexEvent::TurnError { thread_id, .. } => vec![EngineEvent::Lifecycle {
                session: session(&thread_id),
                fact: LifecycleFact::Errored,
            }],
            CodexEvent::TokenUsageUpdated { thread_id, .. } => vec![EngineEvent::CostReported {
                session: session(&thread_id),
            }],
            // Transcript ingestion (axis 3) has no fact in this vocabulary —
            // it is a stream of typed items, not a point on a lifeline — so it
            // surfaces as unmodeled rather than being forced into a variant.
            CodexEvent::ItemStarted { .. } => vec![EngineEvent::Unmodeled {
                tag: "item/started".to_owned(),
            }],
            CodexEvent::ItemCompleted { .. } => vec![EngineEvent::Unmodeled {
                tag: "item/completed".to_owned(),
            }],
            // A resolution is the END of an approval, and axis 4's fact is the
            // ASK. Reporting it as `ApprovalAsked` would double every approval.
            CodexEvent::ApprovalResolved { .. } => vec![EngineEvent::Unmodeled {
                tag: "serverRequest/resolved".to_owned(),
            }],
            CodexEvent::Unrecognized { method } => vec![EngineEvent::Unmodeled { tag: method }],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ThreadActiveFlag, Turn, TurnError, TurnStatus};

    #[test]
    fn a_thread_start_is_a_lifecycle_fact_and_a_status_at_once() {
        let events = CodexAdapter
            .normalize(CodexEvent::ThreadStarted {
                thread_id: "th-1".to_owned(),
                status: ThreadStatus::Idle,
            })
            .expect("infallible");
        assert_eq!(
            events,
            [
                EngineEvent::Lifecycle {
                    session: EngineSessionId::new("th-1"),
                    fact: LifecycleFact::Started,
                },
                EngineEvent::Status {
                    session: EngineSessionId::new("th-1"),
                    status: EngineStatus::Idle,
                },
            ]
        );
    }

    #[test]
    fn waiting_on_approval_beats_active_because_it_is_a_flag_on_active() {
        // codex reports waiting as `active` PLUS a flag, so a mapping that
        // matched on the variant first would report every waiting thread as
        // working — the exact skew axis 2 measures.
        let waiting = ThreadStatus::Active {
            active_flags: vec![ThreadActiveFlag::WaitingOnApproval],
        };
        assert_eq!(status_of(&waiting), EngineStatus::WaitingOnApproval);
        let working = ThreadStatus::Active {
            active_flags: vec![ThreadActiveFlag::WaitingOnUserInput],
        };
        assert_eq!(status_of(&working), EngineStatus::Working);
    }

    #[test]
    fn a_completed_turn_is_a_lifecycle_fact_only_when_it_carries_an_error() {
        // `docs/PARITY.md` lists `turn/completed` in the error column alone.
        // The earlier version of this mapping read a completed turn as idle and
        // an interrupted one as an error, and both were this file's inference
        // rather than the row's claim — the second reader caught it, and the
        // harness had been passing on the same confusion since before the
        // adapter existed (its runner pushed "end" on this notification, which
        // the row assigns to `thread/closed`).
        for (status, error, expected) in [
            (TurnStatus::Completed, None, None),
            (TurnStatus::Interrupted, None, None),
            (TurnStatus::InProgress, None, None),
            (
                TurnStatus::Failed,
                Some(TurnError {
                    message: "boom".to_owned(),
                    additional_details: None,
                }),
                Some(LifecycleFact::Errored),
            ),
        ] {
            let events = CodexAdapter
                .normalize(CodexEvent::TurnCompleted {
                    thread_id: "th-1".to_owned(),
                    turn: Turn {
                        id: "t-1".to_owned(),
                        status,
                        error,
                        items: Vec::new(),
                    },
                })
                .expect("infallible");
            assert_eq!(
                events.first().and_then(EngineEvent::lifecycle),
                expected,
                "{status:?}"
            );
        }
    }

    #[test]
    fn the_idle_fact_comes_from_the_status_push_the_contract_names() {
        let events = CodexAdapter
            .normalize(CodexEvent::StatusChanged {
                thread_id: "th-1".to_owned(),
                status: ThreadStatus::Idle,
            })
            .expect("infallible");
        assert_eq!(
            events
                .iter()
                .filter_map(EngineEvent::lifecycle)
                .collect::<Vec<_>>(),
            [LifecycleFact::Idle]
        );
        // A busy push is a status and nothing more.
        let busy = CodexAdapter
            .normalize(CodexEvent::StatusChanged {
                thread_id: "th-1".to_owned(),
                status: ThreadStatus::Active {
                    active_flags: Vec::new(),
                },
            })
            .expect("infallible");
        assert!(busy.iter().all(|e| e.lifecycle().is_none()), "{busy:?}");
    }

    #[test]
    fn a_resolved_approval_is_not_a_second_ask() {
        let events = CodexAdapter
            .normalize(CodexEvent::ApprovalResolved {
                request_id: crate::JsonRpcId::Num(1),
                thread_id: "th-1".to_owned(),
            })
            .expect("infallible");
        assert!(
            !matches!(events.as_slice(), [EngineEvent::ApprovalAsked { .. }]),
            "a resolution was reported as an ask: {events:?}"
        );
    }
}
