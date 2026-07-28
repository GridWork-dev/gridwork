//! The stream checker: replay any exported event stream against the contract.
//!
//! Verifies envelope structure, sequence monotonicity, aggregate contiguity,
//! full FSM replay (edge legality, version discipline, terminal immutability,
//! the D5 flip actor + receipt rule), the command outcome⇔target agreement,
//! and payload bounds. Findings are typed and stable — CI parses them.
//!
//! **Replay convention** (contract): a state change is an event whose
//! `event_type` ends in `_state_changed` with payload `{"from": s, "to": s}`;
//! creation is `_created` at `aggregate_version` 1. Aggregate types outside
//! the four public FSMs get envelope-level checks only (open world).
//!
//! **Explicit non-claim:** this checker cannot detect historical tampering
//! from stream input alone — a coherent forged stream passes. Tamper evidence
//! requires the storage layer (append-only enforcement + provenance), not
//! stream inspection.

use std::collections::HashMap;

use gwk_domain::envelope::{
    ENVELOPE_SCHEMA_VERSION, EventEnvelope, INLINE_PAYLOAD_MAX_BYTES, accept_schema_version,
};
use gwk_domain::fsm::{AttemptState, CommandState, MessageState, StateMachine, TaskState};
use gwk_domain::ids::{EventId, Seq};
use gwk_domain::transition::LIVENESS_PRODUCER_KIND;

/// What the checker can find. Closed set: CI matches on these.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, specta::Type,
)]
#[serde(rename_all = "snake_case")]
pub enum FindingCode {
    EnvelopeMalformed,
    SchemaVersionUnknown,
    SeqNotIncreasing,
    CreatedNotFirst,
    CreationMissing,
    CreationStateInvalid,
    AggregateVersionGap,
    StateChangeMalformed,
    FromStateMismatch,
    IllegalTransition,
    TerminalMutation,
    FlipWrongActor,
    FlipMissingReceipt,
    IdempotencyDuplicate,
    OutcomeMissing,
    OutcomeOnNonTerminal,
    OutcomeDisagreesWithTargets,
    InlinePayloadTooLarge,
    PayloadRefInvalid,
}

/// One violation, anchored to where it happened.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
pub struct Finding {
    pub code: FindingCode,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub seq: Option<Seq>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub event_id: Option<EventId>,
    pub message: String,
}

struct ReplayState {
    state: String,
    version: u32,
}

/// What creation told us about a stop command: the targets it named, and
/// which of them were ALREADY canceled when it was issued — a later `clean`
/// cannot claim this command stopped those.
struct CommandCtx {
    targets: Vec<String>,
    pre_canceled: Vec<String>,
}

fn payload_str<'a>(event: &'a EventEnvelope, key: &str) -> Option<&'a str> {
    event.payload.get(key).and_then(|v| v.as_str())
}

/// The `targets` array of a command payload, if present and an array.
fn payload_targets(event: &EventEnvelope) -> Option<Vec<String>> {
    event
        .payload
        .get("targets")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|t| t.as_str().map(String::from))
                .collect()
        })
}

/// The declared initial state of a machine-governed aggregate type.
/// Returns None when `aggregate_type` is not FSM-governed (open world).
fn machine_initial(aggregate_type: &str) -> Option<&'static str> {
    Some(match aggregate_type {
        "task" => "submitted",
        "attempt" => "queued",
        "message" => "accepted",
        "command" => "issued",
        _ => return None,
    })
}

/// Edge legality + terminality for one of the four public FSMs, by wire name.
/// Returns None when `aggregate_type` is not FSM-governed (open world).
fn fsm_check(aggregate_type: &str, from: &str, to: &str) -> Option<(bool, bool)> {
    fn check<S: StateMachine + serde::de::DeserializeOwned>(from: &str, to: &str) -> (bool, bool) {
        let parse = |name: &str| -> Option<S> {
            serde_json::from_value(serde_json::Value::String(name.to_string())).ok()
        };
        match (parse(from), parse(to)) {
            (Some(f), Some(t)) => (S::can_transition(f, t), f.is_terminal()),
            // An unknown state name is an illegal transition by definition.
            _ => (false, false),
        }
    }
    match aggregate_type {
        "task" => Some(check::<TaskState>(from, to)),
        "attempt" => Some(check::<AttemptState>(from, to)),
        "message" => Some(check::<MessageState>(from, to)),
        "command" => Some(check::<CommandState>(from, to)),
        _ => None,
    }
}

/// Replay `events` (in stream order) and return every violation found.
pub fn check_stream(events: &[EventEnvelope]) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut last_seq: Option<u64> = None;
    let mut replay: HashMap<(String, String), ReplayState> = HashMap::new();
    let mut seen_keys: HashMap<(String, String, String), ()> = HashMap::new();
    // command aggregate id -> what creation declared
    let mut commands: HashMap<String, CommandCtx> = HashMap::new();

    for event in events {
        let at = |code: FindingCode, message: String| Finding {
            code,
            seq: Some(event.global_sequence),
            event_id: Some(event.event_id.clone()),
            message,
        };

        // -- envelope-level checks --
        if let Err(err) = accept_schema_version(event.schema_version, ENVELOPE_SCHEMA_VERSION, &[])
        {
            findings.push(at(FindingCode::SchemaVersionUnknown, err.to_string()));
        }
        let seq = event.global_sequence.value();
        if let Some(prev) = last_seq
            && seq <= prev
        {
            findings.push(at(
                FindingCode::SeqNotIncreasing,
                format!("global_sequence {seq} after {prev} (must be strictly increasing; gaps are fine)"),
            ));
        }
        last_seq = Some(seq);
        let inline_len = serde_json::to_string(&event.payload)
            .map(|s| s.len())
            .unwrap_or(usize::MAX);
        if inline_len > INLINE_PAYLOAD_MAX_BYTES {
            findings.push(at(
                FindingCode::InlinePayloadTooLarge,
                format!("inline payload {inline_len} bytes exceeds the {INLINE_PAYLOAD_MAX_BYTES}-byte bound"),
            ));
        }
        if let Some(payload_ref) = &event.payload_ref
            && (!payload_ref.digest.contains(':') || payload_ref.media_type.is_empty())
        {
            findings.push(at(
                FindingCode::PayloadRefInvalid,
                "payload_ref needs a <scheme>:<address> digest and a media_type".to_string(),
            ));
        }
        if let Some(key) = &event.idempotency_key {
            let full_key = (
                event.aggregate_type.clone(),
                event.aggregate_id.0.clone(),
                key.0.clone(),
            );
            if seen_keys.insert(full_key, ()).is_some() {
                findings.push(at(
                    FindingCode::IdempotencyDuplicate,
                    format!("idempotency_key {key} appears twice for this aggregate — a retry must not append twice"),
                ));
            }
        }

        // -- aggregate replay --
        let agg_key = (event.aggregate_type.clone(), event.aggregate_id.0.clone());
        let expected_version = replay.get(&agg_key).map(|r| r.version + 1).unwrap_or(1);
        if event.aggregate_version != expected_version {
            findings.push(at(
                FindingCode::AggregateVersionGap,
                format!(
                    "aggregate_version {} but replay expects {expected_version} (contiguous from 1)",
                    event.aggregate_version
                ),
            ));
        }
        // The version cursor advances on EVERY event of a tracked aggregate —
        // state-bearing or not — so a non-state event counts toward contiguity
        // and a reused or rewound aggregate_version cannot hide behind one.
        if let Some(current) = replay.get_mut(&agg_key) {
            current.version = event.aggregate_version;
        }
        // A machine-governed aggregate's history begins at its creation event;
        // anything else fabricates an aggregate mid-stream and skips the ladder.
        let is_creation = event.event_type.ends_with("_created");
        if !is_creation
            && machine_initial(&event.aggregate_type).is_some()
            && !replay.contains_key(&agg_key)
        {
            findings.push(at(
                FindingCode::CreationMissing,
                format!(
                    "first event for this {} is {} — a machine-governed aggregate starts at its creation event",
                    event.aggregate_type, event.event_type
                ),
            ));
        }

        if is_creation {
            if replay.contains_key(&agg_key) {
                findings.push(at(
                    FindingCode::CreatedNotFirst,
                    "creation event on an aggregate that already exists".to_string(),
                ));
            }
            let seeded = payload_str(event, "state");
            let initial = match machine_initial(&event.aggregate_type) {
                // A machine-governed aggregate is born in its declared initial
                // state, nothing else; replay proceeds from the lawful one.
                Some(want) => {
                    if let Some(seeded) = seeded
                        && seeded != want
                    {
                        findings.push(at(
                            FindingCode::CreationStateInvalid,
                            format!(
                                "created with state {seeded} but a {} begins at {want}",
                                event.aggregate_type
                            ),
                        ));
                    }
                    want
                }
                None => seeded.unwrap_or(""),
            };
            replay.insert(
                agg_key.clone(),
                ReplayState {
                    state: initial.to_string(),
                    version: event.aggregate_version,
                },
            );
            if event.aggregate_type == "command" {
                let targets = payload_targets(event).unwrap_or_default();
                let pre_canceled = targets
                    .iter()
                    .filter(|t| {
                        replay
                            .get(&("attempt".to_string(), (*t).clone()))
                            .is_some_and(|r| r.state == "canceled")
                    })
                    .cloned()
                    .collect();
                commands.insert(
                    event.aggregate_id.0.clone(),
                    CommandCtx {
                        targets,
                        pre_canceled,
                    },
                );
            }
        } else if event.event_type.ends_with("_state_changed") {
            let (Some(from), Some(to)) = (payload_str(event, "from"), payload_str(event, "to"))
            else {
                findings.push(at(
                    FindingCode::StateChangeMalformed,
                    "state-change payload must carry string `from` and `to`".to_string(),
                ));
                continue;
            };
            if let Some(current) = replay.get(&agg_key)
                && current.state != from
            {
                findings.push(at(
                    FindingCode::FromStateMismatch,
                    format!("payload.from is {from} but replay says {}", current.state),
                ));
            }
            match fsm_check(&event.aggregate_type, from, to) {
                Some((_, true)) => {
                    findings.push(at(
                        FindingCode::TerminalMutation,
                        format!("{from} is terminal; nothing leaves a terminal"),
                    ));
                }
                Some((false, false)) => {
                    findings.push(at(
                        FindingCode::IllegalTransition,
                        format!(
                            "{}: {from} -> {to} is not a legal edge",
                            event.aggregate_type
                        ),
                    ));
                }
                _ => {}
            }
            // D5: the blocked flip demands the liveness producer and a receipt.
            if event.aggregate_type == "attempt"
                && ((from == "running" && to == "blocked")
                    || (from == "blocked" && to == "running"))
            {
                if event.actor.kind != LIVENESS_PRODUCER_KIND {
                    findings.push(at(
                        FindingCode::FlipWrongActor,
                        format!(
                            "running<->blocked flip by actor kind {} — only {LIVENESS_PRODUCER_KIND} may write it",
                            event.actor.kind
                        ),
                    ));
                }
                if payload_str(event, "receipt_id").is_none_or(str::is_empty) {
                    findings.push(at(
                        FindingCode::FlipMissingReceipt,
                        "running<->blocked flip without a receipt_id".to_string(),
                    ));
                }
            }
            // Command outcome discipline (ruling: outcome iff terminal,
            // written with the terminal transition).
            if event.aggregate_type == "command" {
                let outcome = payload_str(event, "outcome");
                if to == "verification_complete" {
                    match outcome {
                        None => findings.push(at(
                            FindingCode::OutcomeMissing,
                            "verification_complete without an outcome in the same event"
                                .to_string(),
                        )),
                        Some("clean") => {
                            // The agreement is judged HERE, at the terminal
                            // event: replay holds every target's state as of
                            // this point in the stream, not at stream end.
                            // `clean` claims THIS command verifiably stopped
                            // its targets: each must be `canceled` right now,
                            // and not already canceled before it was issued.
                            let ctx = commands.get(&event.aggregate_id.0);
                            // Prefer the terminal event's own targets, else
                            // the ones carried from the command's creation.
                            let targets = payload_targets(event)
                                .or_else(|| ctx.map(|c| c.targets.clone()))
                                .unwrap_or_default();
                            if targets.is_empty() {
                                findings.push(at(
                                    FindingCode::OutcomeDisagreesWithTargets,
                                    "outcome clean but the command names no targets to agree with"
                                        .to_string(),
                                ));
                            }
                            for target in &targets {
                                let state = replay
                                    .get(&("attempt".to_string(), target.clone()))
                                    .map(|r| r.state.as_str());
                                if state != Some("canceled") {
                                    findings.push(at(
                                        FindingCode::OutcomeDisagreesWithTargets,
                                        format!(
                                            "outcome clean but target {target} is {} at the terminal event",
                                            state.unwrap_or("<absent>")
                                        ),
                                    ));
                                } else if ctx.is_some_and(|c| c.pre_canceled.contains(target)) {
                                    findings.push(at(
                                        FindingCode::OutcomeDisagreesWithTargets,
                                        format!(
                                            "outcome clean but target {target} was already canceled when the command was created"
                                        ),
                                    ));
                                }
                            }
                        }
                        Some(_) => {}
                    }
                } else if outcome.is_some() {
                    findings.push(at(
                        FindingCode::OutcomeOnNonTerminal,
                        format!("outcome on a non-terminal transition to {to}"),
                    ));
                }
            }
            if let Some(current) = replay.get_mut(&agg_key) {
                current.state = to.to_string();
            } else {
                replay.insert(
                    agg_key.clone(),
                    ReplayState {
                        state: to.to_string(),
                        version: event.aggregate_version,
                    },
                );
            }
        }
    }

    findings
}

/// Parse a JSON stream (array of envelopes) leniently: a malformed element
/// becomes a finding instead of aborting the whole run.
pub fn parse_stream(raw: &str) -> (Vec<EventEnvelope>, Vec<Finding>) {
    let mut findings = Vec::new();
    let values: Vec<serde_json::Value> = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(err) => {
            findings.push(Finding {
                code: FindingCode::EnvelopeMalformed,
                seq: None,
                event_id: None,
                message: format!("stream is not a JSON array of envelopes: {err}"),
            });
            return (Vec::new(), findings);
        }
    };
    let mut events = Vec::with_capacity(values.len());
    for (index, value) in values.into_iter().enumerate() {
        match serde_json::from_value::<EventEnvelope>(value) {
            Ok(event) => events.push(event),
            Err(err) => findings.push(Finding {
                code: FindingCode::EnvelopeMalformed,
                seq: None,
                event_id: None,
                message: format!("envelope[{index}]: {err}"),
            }),
        }
    }
    (events, findings)
}
