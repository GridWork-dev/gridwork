//! [`EngineAdapter`] for codex: the app-server's vocabulary, reduced to the
//! four facts `docs/PARITY.md` asks every engine for.
//!
//! [`CodexEvent`] stays the crate's own richer surface — it carries turn ids,
//! typed items, retry intent, and token usage that a caller driving codex
//! specifically needs. This module is the lossy half on purpose: what survives
//! is what a supervisor can ask of all three engines without knowing which one
//! it has.
//!
//! # Clean-room scope
//!
//! No new protocol behavior is read here. Every mapping below is over
//! [`CodexEvent`], which this crate already derived and cited; the derivations
//! for `thread/started`, `thread/status/changed`, `turn/completed` and the rest
//! sit at their parse sites in [`crate::event`] and [`crate::schema`]. A
//! `Derivation:` marker on a match arm would cite the row a sibling module
//! already carries for the same fact, which is how a citation becomes decor.

use gwk_domain::engine::{EngineAdapter, EngineEvent, EngineStatus, LifecycleFact};
use gwk_domain::ids::{EngineId, EngineSessionId};

use crate::event::CodexEvent;
use crate::schema::{ThreadStatus, TurnStatus};

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
/// `notLoaded` and `systemError` are not statuses a supervisor acts on
/// differently from idle: neither says the engine is working, and the
/// systemError case reaches a caller as a lifecycle error through its own
/// notification rather than as a status. Collapsing them here loses nothing the
/// caller could have used, and `CodexEvent` still carries the exact
/// `ThreadStatus` for anything that wants it.
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
            CodexEvent::StatusChanged { thread_id, status } => vec![EngineEvent::Status {
                session: session(&thread_id),
                status: status_of(&status),
            }],
            CodexEvent::ThreadClosed { thread_id } => vec![EngineEvent::Lifecycle {
                session: session(&thread_id),
                fact: LifecycleFact::Ended,
            }],
            // The one arm that is genuinely two facts: axis 1 reads a completed
            // turn as idle, and a turn that ended any other way as an error.
            // `inProgress` on a `turn/completed` is neither — it is a
            // contradiction the app-server should not send, and reporting
            // nothing is honest where inventing `idle` would not be.
            CodexEvent::TurnCompleted { thread_id, turn } => match turn.status {
                TurnStatus::Completed => vec![EngineEvent::Lifecycle {
                    session: session(&thread_id),
                    fact: LifecycleFact::Idle,
                }],
                TurnStatus::Interrupted | TurnStatus::Failed => vec![EngineEvent::Lifecycle {
                    session: session(&thread_id),
                    fact: LifecycleFact::Errored,
                }],
                TurnStatus::InProgress => Vec::new(),
            },
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
    use crate::schema::{ThreadActiveFlag, Turn};

    fn turn(status: TurnStatus) -> Turn {
        Turn {
            id: "t-1".to_owned(),
            status,
            error: None,
            items: Vec::new(),
        }
    }

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
    fn a_turns_ending_decides_between_idle_and_error() {
        for (status, expected) in [
            (TurnStatus::Completed, Some(LifecycleFact::Idle)),
            (TurnStatus::Interrupted, Some(LifecycleFact::Errored)),
            (TurnStatus::Failed, Some(LifecycleFact::Errored)),
            // A `turn/completed` that says the turn is still running is a
            // contradiction; nothing is the honest answer.
            (TurnStatus::InProgress, None),
        ] {
            let events = CodexAdapter
                .normalize(CodexEvent::TurnCompleted {
                    thread_id: "th-1".to_owned(),
                    turn: turn(status),
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
