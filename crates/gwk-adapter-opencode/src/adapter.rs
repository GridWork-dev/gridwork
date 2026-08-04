//! [`EngineAdapter`] for opencode: the server event bus, reduced to the four
//! facts `docs/PARITY.md` asks every engine for.
//!
//! This crate is the one that speaks a real ACP wire, and it is worth being
//! precise about why that does not make it the normalization layer. ACP is how
//! this engine's agent loop is driven; lifecycle, status and approval do not
//! ride that connection at all — they ride the server's own event bus, per
//! `docs/PARITY.md`. So even here the convergence is on gwk's types, not on the
//! ACP SDK's, and the sibling adapters are not approximating something this one
//! gets natively.
//!
//! # Clean-room scope
//!
//! No new protocol behavior is read here. Every mapping below is over
//! [`Event`], which this crate already derived and cited against
//! `OPENCODE-SERVER`; the derivations sit at their parse sites in
//! [`crate::event`].

// Derivation: none — this module reads no protocol. Every value it matches on is
// already parsed and already cited at its parse site in a sibling module, and
// what it decides is which entry of `docs/PARITY.md`'s own axis tables each one
// belongs to. That is a mapping onto this repository's contract, not a reading
// of a vendor's, so there is no source to name — and naming one would put a
// citation on a decision the cited page never made.

use gwk_domain::engine::{EngineAdapter, EngineEvent, EngineStatus, LifecycleFact};
use gwk_domain::ids::{EngineId, EngineSessionId};

use crate::event::{Event, SessionStatus};

/// The opencode adapter's normalization half.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct OpencodeAdapter;

fn session(session_id: &str) -> EngineSessionId {
    EngineSessionId::new(session_id)
}

/// The three states axis 2 allows, from opencode's three.
///
/// `retry` is working. The engine is mid-attempt with a next time already
/// scheduled, and a supervisor that read it as idle would offer the session
/// work while it was busy failing. The attempt count and delay stay on
/// [`Event`] for anything that wants to reason about the retry itself.
fn status_of(status: &SessionStatus) -> EngineStatus {
    match status {
        SessionStatus::Idle => EngineStatus::Idle,
        SessionStatus::Busy | SessionStatus::Retry { .. } => EngineStatus::Working,
    }
}

impl EngineAdapter for OpencodeAdapter {
    type Raw = Event;
    /// Infallible for the same reason codex's is: [`Event`] is the parsed,
    /// validated form, and every failure this crate can have was raised as
    /// [`crate::event::EventParseError`] before one existed.
    type Error = std::convert::Infallible;

    fn engine_id(&self) -> EngineId {
        crate::engine_id()
    }

    fn normalize(&self, raw: Event) -> Result<Vec<EngineEvent>, Self::Error> {
        Ok(match raw {
            Event::SessionCreated { session_id, .. } => vec![EngineEvent::Lifecycle {
                session: session(&session_id),
                fact: LifecycleFact::Started,
            }],
            Event::SessionIdle { session_id } => vec![EngineEvent::Lifecycle {
                session: session(&session_id),
                fact: LifecycleFact::Idle,
            }],
            Event::SessionError { session_id, .. } => vec![EngineEvent::Lifecycle {
                session: session(&session_id),
                fact: LifecycleFact::Errored,
            }],
            Event::SessionDeleted { session_id } => vec![EngineEvent::Lifecycle {
                session: session(&session_id),
                fact: LifecycleFact::Ended,
            }],
            Event::SessionStatusChanged { session_id, status } => vec![EngineEvent::Status {
                session: session(&session_id),
                status: status_of(&status),
            }],
            Event::PermissionAsked(ask) => vec![EngineEvent::ApprovalAsked {
                session: session(&ask.session_id),
                request: ask.request_id,
            }],
            // The reply is the end of an approval, not a second ask — the same
            // line the codex adapter draws at `serverRequest/resolved`.
            Event::PermissionReplied { .. } => vec![EngineEvent::Unmodeled {
                tag: "permission.replied".to_owned(),
            }],
            // Stream liveness is a fact about the CONNECTION, and every other
            // variant here is a fact about a session. It has no session to
            // attribute to, which is exactly what `Unmodeled` carries.
            Event::StreamConnected => vec![EngineEvent::Unmodeled {
                tag: "server.connected".to_owned(),
            }],
            Event::Other { r#type } => vec![EngineEvent::Unmodeled { tag: r#type }],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_four_bus_events_axis_one_names_map_to_the_four_facts() {
        // `docs/PARITY.md` axis 1's opencode row, read straight across:
        // session.created / session.idle / session.error / session.deleted.
        let cases = [
            (
                Event::SessionCreated {
                    session_id: "s-1".to_owned(),
                    parent_id: None,
                },
                LifecycleFact::Started,
            ),
            (
                Event::SessionIdle {
                    session_id: "s-1".to_owned(),
                },
                LifecycleFact::Idle,
            ),
            (
                Event::SessionError {
                    session_id: "s-1".to_owned(),
                    error: None,
                },
                LifecycleFact::Errored,
            ),
            (
                Event::SessionDeleted {
                    session_id: "s-1".to_owned(),
                },
                LifecycleFact::Ended,
            ),
        ];
        for (event, expected) in cases {
            let normalized = OpencodeAdapter.normalize(event).expect("infallible");
            assert_eq!(
                normalized,
                [EngineEvent::Lifecycle {
                    session: EngineSessionId::new("s-1"),
                    fact: expected,
                }]
            );
        }
    }

    #[test]
    fn a_retrying_session_is_working_not_idle() {
        // The engine is mid-attempt with a next one scheduled. Reading this as
        // idle would hand the session new work while it is busy failing.
        let retry = SessionStatus::Retry {
            attempt: 2,
            message: "rate limited".to_owned(),
            next: 1_000,
        };
        assert_eq!(status_of(&retry), EngineStatus::Working);
        assert_eq!(status_of(&SessionStatus::Busy), EngineStatus::Working);
        assert_eq!(status_of(&SessionStatus::Idle), EngineStatus::Idle);
    }

    #[test]
    fn stream_liveness_is_not_attributed_to_a_session() {
        let events = OpencodeAdapter
            .normalize(Event::StreamConnected)
            .expect("infallible");
        assert_eq!(events.len(), 1);
        // It is a fact about the connection. Attributing it to a session would
        // be inventing one, and every other variant here is genuinely about a
        // session — that difference is what `session()` returning `None` says.
        assert_eq!(events[0].session(), None);
    }
}
