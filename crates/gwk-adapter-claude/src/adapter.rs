//! [`EngineAdapter`] for claude: two channels, reduced to the four facts
//! `docs/PARITY.md` asks every engine for.
//!
//! This adapter is the one where the facts do not arrive on a single surface.
//! Codex and opencode each push lifecycle, status and approval down one stream;
//! claude splits them, because the engine draws its own TUI and the only
//! decision-returning path that exists in that arrangement is its hook
//! interface. So [`ClaudeSignal`] is a union of the two, and normalizing is the
//! same operation over either.
//!
//! # The idle gap, stated rather than papered over
//!
//! `docs/PARITY.md` axis 1 gives claude's idle fact as the `Stop` hook. This
//! crate does not model that hook — [`crate::hook`] models `PreToolUse`, the
//! approval channel, and nothing else. So [`ClaudeAdapter`] reports three of
//! the four facts and **not** idle.
//!
//! The tempting mapping is `result` → idle, since a `result` message does mean
//! the engine stopped working. It is wrong, and wrong in the direction that
//! costs the most: axis 1 separates idle from end precisely because a turn
//! finishing inside a live session is a different fact from the session being
//! over, and in the bidirectional stream those are different moments. Reporting
//! `result` as both would make the harness's claude row pass while the fact it
//! checks was never observed — the exact shape of certification-without-a-
//! referent this trait exists to end. `a_result_is_the_end_and_not_also_idle`
//! pins the absence so it cannot be closed by accident.
//!
//! # Clean-room scope
//!
//! No new protocol behavior is read here. Every mapping is over [`Message`] and
//! [`PreToolUseAsk`], which this crate already derived and cited against
//! `CLAUDE-HEADLESS`, `CLAUDE-AGENT-SDK` and `CLAUDE-HOOKS`; the derivations sit
//! at their parse sites in [`crate::message`] and [`crate::hook`]. In
//! particular the success/error split below rests on `Message::is_result` and
//! `Message::is_successful_result`, whose markers carry the `SDKResultMessage`
//! subtype union this arm depends on.

// Derivation: none — this module reads no protocol. Every value it matches on is
// already parsed and already cited at its parse site in a sibling module, and
// what it decides is which entry of `docs/PARITY.md`'s own axis tables each one
// belongs to. That is a mapping onto this repository's contract, not a reading
// of a vendor's, so there is no source to name — and naming one would put a
// citation on a decision the cited page never made.

use gwk_domain::engine::{EngineAdapter, EngineEvent, LifecycleFact};
use gwk_domain::ids::{EngineId, EngineSessionId};

use crate::hook::PreToolUseAsk;
use crate::message::Message;

/// One signal from either of claude's two control channels.
///
/// A union rather than two trait implementations: an implementation per channel
/// would make `engine_id()` answerable twice for one engine, and a supervisor
/// holding "the claude adapter" would have to know which half it had.
#[derive(Debug, Clone, PartialEq)]
pub enum ClaudeSignal {
    /// A line off the bidirectional stream-json transport.
    Stream(Message),
    /// A `PreToolUse` ask arriving on the hook channel.
    Ask(PreToolUseAsk),
}

/// The claude adapter's normalization half.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ClaudeAdapter;

/// Why a signal carried no attributable fact.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NormalizeError {
    /// A stream message that should name a session did not.
    ///
    /// Fatal rather than skipped: `system`/`init` and `result` are exactly the
    /// messages whose whole job is to bracket a session, and one arriving
    /// without a `session_id` means the transport is not what this adapter
    /// believes it is. Dropping it would hide that behind a quiet gap in the
    /// lifeline.
    #[error("a {kind} message carried no session_id")]
    UnattributedLifecycle { kind: &'static str },
}

impl EngineAdapter for ClaudeAdapter {
    type Raw = ClaudeSignal;
    type Error = NormalizeError;

    fn engine_id(&self) -> EngineId {
        crate::engine_id()
    }

    fn normalize(&self, raw: ClaudeSignal) -> Result<Vec<EngineEvent>, NormalizeError> {
        match raw {
            ClaudeSignal::Ask(ask) => Ok(vec![EngineEvent::ApprovalAsked {
                session: EngineSessionId::new(ask.session_id),
                request: ask.tool_use_id,
            }]),
            ClaudeSignal::Stream(message) => Self::normalize_stream(&message),
        }
    }
}

impl ClaudeAdapter {
    fn normalize_stream(message: &Message) -> Result<Vec<EngineEvent>, NormalizeError> {
        let session = |kind: &'static str| -> Result<EngineSessionId, NormalizeError> {
            message
                .session_id
                .as_deref()
                .map(EngineSessionId::new)
                .ok_or(NormalizeError::UnattributedLifecycle { kind })
        };

        if message.is_session_start() {
            return Ok(vec![EngineEvent::Lifecycle {
                session: session("system/init")?,
                fact: LifecycleFact::Started,
            }]);
        }

        if message.is_result() {
            let session = session("result")?;
            let mut events = Vec::with_capacity(2);
            // The error fact precedes the end fact, because that is the order
            // they happened in: the run failed and therefore finished. A
            // consumer folding these in order sees a failed session end, not a
            // session that ended and then failed.
            if !message.is_successful_result() {
                events.push(EngineEvent::Lifecycle {
                    session: session.clone(),
                    fact: LifecycleFact::Errored,
                });
            }
            events.push(EngineEvent::Lifecycle {
                session: session.clone(),
                fact: LifecycleFact::Ended,
            });
            events.push(EngineEvent::CostReported { session });
            // A `result` is also where the invocation's spend is reported.
            // Unconditionally, not gated on `total_cost_usd`: axis 3 is explicit
            // that "token counts are the universal fact; currency is not", so
            // keying the report on the currency figure would drop the spend of
            // every engine invocation that reported usage without a dollar
            // amount — and `cost::record_cost_entry` already takes that figure
            // as an `Option`. Which fields are recordable is that module's
            // decision; this one only says a report arrived.
            return Ok(events);
        }

        // Assistant text, user turns, tool blocks, subagent messages, the
        // plugin/hook startup events — real traffic, none of it a fact in this
        // vocabulary. `kind` rather than a constant, so a log says which.
        Ok(vec![EngineEvent::Unmodeled {
            tag: message.kind.clone(),
        }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gwk_domain::engine::EngineEvent;

    fn message(json: serde_json::Value) -> Message {
        serde_json::from_value(json).expect("a Message")
    }

    #[test]
    fn system_init_is_the_start_fact() {
        let events = ClaudeAdapter
            .normalize(ClaudeSignal::Stream(message(serde_json::json!({
                "type": "system",
                "subtype": "init",
                "session_id": "s-1",
            }))))
            .expect("normalizes");
        assert_eq!(
            events,
            [EngineEvent::Lifecycle {
                session: EngineSessionId::new("s-1"),
                fact: LifecycleFact::Started,
            }]
        );
    }

    #[test]
    fn a_result_is_the_end_and_not_also_idle() {
        // The gap this file's docs state, pinned. `docs/PARITY.md` gives
        // claude's idle fact as the `Stop` hook, which this crate does not
        // model; mapping `result` to idle as well would make the harness's
        // claude lifecycle row pass on a fact nothing observed. If the `Stop`
        // hook is ever modeled, THAT is what closes this — not this arm.
        let events = ClaudeAdapter
            .normalize(ClaudeSignal::Stream(message(serde_json::json!({
                "type": "result",
                "subtype": "success",
                "session_id": "s-1",
            }))))
            .expect("normalizes");
        let facts: Vec<LifecycleFact> = events.iter().filter_map(EngineEvent::lifecycle).collect();
        assert_eq!(facts, [LifecycleFact::Ended]);
        assert!(
            !facts.contains(&LifecycleFact::Idle),
            "a result was reported as idle: {events:?}"
        );
    }

    #[test]
    fn a_failed_result_reports_the_error_before_the_end() {
        let events = ClaudeAdapter
            .normalize(ClaudeSignal::Stream(message(serde_json::json!({
                "type": "result",
                "subtype": "error_during_execution",
                "session_id": "s-1",
                "is_error": true,
            }))))
            .expect("normalizes");
        let facts: Vec<LifecycleFact> = events.iter().filter_map(EngineEvent::lifecycle).collect();
        // Order is the claim: it failed and therefore finished.
        assert_eq!(facts, [LifecycleFact::Errored, LifecycleFact::Ended]);
    }

    #[test]
    fn spend_is_reported_on_a_result_with_or_without_a_currency_figure() {
        // This was gated on `total_cost_usd` and the second reader caught it:
        // axis 3 says token counts are the universal fact and currency is not,
        // so keying the report on the dollar amount drops the spend of every
        // invocation that reported usage without one.
        for json in [
            serde_json::json!({"type": "result", "subtype": "success", "session_id": "s-1"}),
            serde_json::json!({
                "type": "result", "subtype": "success", "session_id": "s-1",
                "total_cost_usd": 0.42,
            }),
        ] {
            let events = ClaudeAdapter
                .normalize(ClaudeSignal::Stream(message(json.clone())))
                .expect("normalizes");
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e, EngineEvent::CostReported { .. })),
                "{json}: dropped the spend report"
            );
        }
    }

    #[test]
    fn a_lifecycle_message_without_a_session_is_a_fault_not_a_gap() {
        // Silently skipping this would leave a hole in the lifeline that reads
        // exactly like an engine that never started.
        for json in [
            serde_json::json!({"type": "system", "subtype": "init"}),
            serde_json::json!({"type": "result", "subtype": "success"}),
        ] {
            let error = ClaudeAdapter
                .normalize(ClaudeSignal::Stream(message(json.clone())))
                .expect_err(&format!("{json} should not normalize"));
            assert!(matches!(
                error,
                NormalizeError::UnattributedLifecycle { .. }
            ));
        }
    }

    #[test]
    fn ordinary_traffic_is_unmodeled_and_says_what_it_was() {
        let events = ClaudeAdapter
            .normalize(ClaudeSignal::Stream(message(serde_json::json!({
                "type": "assistant",
                "session_id": "s-1",
            }))))
            .expect("normalizes");
        assert_eq!(
            events,
            [EngineEvent::Unmodeled {
                tag: "assistant".to_owned()
            }]
        );
    }
}
