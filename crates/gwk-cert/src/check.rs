//! The stream checker: replay any exported event stream against the contract.
//!
//! Verifies envelope structure, sequence monotonicity, aggregate contiguity,
//! full FSM replay (edge legality, version discipline, terminal immutability,
//! the liveness-producer flip actor + receipt rule), the command
//! outcome⇔target agreement, and payload bounds. Findings are typed and
//! stable — CI parses them.
//!
//! **Replay convention** (contract): a state change is an event whose
//! `event_type` ends in `_state_changed` with payload `{"from": s, "to": s}`;
//! creation is `_created` at `aggregate_version` 1. A stop command carries
//! `targets: [aggregate_id]` on its `_created` payload (and may restate it on
//! the terminal event); that creation set is the FLOOR a `clean` outcome must
//! have verifiably stopped. Aggregate types outside the four public FSMs get
//! envelope-level checks only (open world).
//!
//! **Explicit non-claim:** this checker cannot detect historical tampering
//! from stream input alone — a coherent forged stream passes. Tamper evidence
//! requires the storage layer (append-only enforcement + provenance), not
//! stream inspection.

use std::collections::{HashMap, HashSet};

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

/// Fold the common Cyrillic/Greek homoglyphs of the ASCII letters that spell
/// the four governed FSM names, so a lookalike ("тask", "mеssage") cannot slip
/// into the open world and skip the ladder. Curated, not exhaustive: an
/// unmapped confusable still passes — no worse than an un-normalized spelling
/// did before — so this only ever pulls MORE evasions onto the ladder. Swap in
/// a Unicode TR39 confusable skeleton if the threat surface widens. ASCII input
/// is returned unchanged (`g` and `n` have no confident lowercase homoglyph, so
/// a lookalike must keep those two letters ASCII to fold onto a governed name).
fn fold_confusables(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'а' | 'α' => 'a', // U+0430 Cyrillic a · U+03B1 Greek alpha
            'е' | 'ε' => 'e', // U+0435 Cyrillic ie · U+03B5 Greek epsilon
            'о' | 'ο' => 'o', // U+043E Cyrillic o · U+03BF Greek omicron
            'с' => 'c',       // U+0441 Cyrillic es
            'р' | 'ρ' => 'p', // U+0440 Cyrillic er · U+03C1 Greek rho
            'т' | 'τ' => 't', // U+0442 Cyrillic te · U+03C4 Greek tau
            'к' | 'κ' => 'k', // U+043A Cyrillic ka · U+03BA Greek kappa
            'м' | 'μ' => 'm', // U+043C Cyrillic em · U+03BC Greek mu
            'ѕ' => 's',       // U+0455 Cyrillic dze
            'ԁ' => 'd',       // U+0501 Cyrillic komi de
            _ => c,
        })
        .collect()
}

/// Replay `events` (in stream order) and return every violation found.
pub fn check_stream(events: &[EventEnvelope]) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut last_seq: Option<u64> = None;
    let mut replay: HashMap<(String, String), ReplayState> = HashMap::new();
    let mut seen_keys: HashMap<(String, String, String), ()> = HashMap::new();
    // Aggregates that have had their `_created` event — the ledger that keys
    // CreatedNotFirst and CreationMissing, kept separate from `replay` (which
    // now tracks EVERY aggregate for version contiguity, created or not).
    let mut seen_creation: HashSet<(String, String)> = HashSet::new();
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
        // A governed FSM is keyed by its canonical (trimmed, ascii-lowercase)
        // aggregate_type. A non-canonical spelling — ASCII case/whitespace
        // ("Task", "task ") OR a Cyrillic/Greek homoglyph ("тask") — must NOT
        // slip into the open world and get envelope checks only. Normalize
        // (trim + lowercase, then fold confusables only if that reveals a
        // governed name) before every FSM dispatch, and red the malformed
        // spelling itself so any such evasion still runs the full ladder.
        let trimmed = event.aggregate_type.trim().to_ascii_lowercase();
        let agg_type = if machine_initial(&trimmed).is_some() {
            trimmed
        } else {
            let folded = fold_confusables(&trimmed);
            if machine_initial(&folded).is_some() {
                folded
            } else {
                trimmed
            }
        };
        if agg_type != event.aggregate_type && machine_initial(&agg_type).is_some() {
            findings.push(at(
                FindingCode::EnvelopeMalformed,
                format!(
                    "aggregate_type {:?} is a non-canonical spelling of the governed FSM {agg_type:?}",
                    event.aggregate_type
                ),
            ));
        }
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
        // The version cursor advances on EVERY event of ANY aggregate the
        // checker sees — governed or open-world, state-bearing or not — so a
        // non-state event counts toward contiguity (an open-world type's
        // versions 1,2,3 no longer false-red a gap) and a reused or rewound
        // aggregate_version cannot hide behind one. `first_seen` is captured
        // before the entry lands: this is the aggregate's first event.
        let first_seen = !replay.contains_key(&agg_key);
        replay
            .entry(agg_key.clone())
            .and_modify(|current| current.version = event.aggregate_version)
            .or_insert_with(|| ReplayState {
                state: String::new(),
                version: event.aggregate_version,
            });
        // A machine-governed aggregate's history begins at its creation event;
        // anything else fabricates an aggregate mid-stream and skips the ladder.
        // Reported ONCE, on that first event — not once per following event.
        let is_creation = event.event_type.ends_with("_created");
        if first_seen && !is_creation && machine_initial(&agg_type).is_some() {
            findings.push(at(
                FindingCode::CreationMissing,
                format!(
                    "first event for this {} is {} — a machine-governed aggregate starts at its creation event",
                    event.aggregate_type, event.event_type
                ),
            ));
        }

        if is_creation {
            let already_created = seen_creation.contains(&agg_key);
            if already_created {
                findings.push(at(
                    FindingCode::CreatedNotFirst,
                    "creation event on an aggregate that already exists".to_string(),
                ));
            }
            let seeded = payload_str(event, "state");
            let initial = match machine_initial(&agg_type) {
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
            // A duplicate creation must NOT reset an existing aggregate's
            // replay state to its initial one — that would erase a terminal and
            // mask the mutation that followed. Seed the state only on the first
            // real creation; the version cursor was already advanced above.
            if !already_created {
                seen_creation.insert(agg_key.clone());
                if let Some(current) = replay.get_mut(&agg_key) {
                    current.state = initial.to_string();
                }
            }
            if agg_type == "command" {
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
            // FromStateMismatch is a claim about a KNOWN prior state. `replay`
            // now holds a version-only placeholder (state = "") for every
            // aggregate on first sight — open-world types and any governed
            // aggregate whose creation we never saw included. Neither has an
            // established state to disagree with, so only compare once a
            // creation has seeded the real one: an open-world aggregate whose
            // first event is a `_state_changed` no longer false-reds a mismatch
            // against "".
            //
            // This gate is DELIBERATELY broader than the pre-placeholder
            // behaviour, and the earlier claim that it was "exactly the set
            // `replay` held" was wrong — that baseline populated `replay` from
            // state changes too, so it also caught a chain that disagreed with
            // ITSELF: `pending -> settled` followed by `pending -> archived` on
            // an aggregate no creation was ever seen for. Under this gate that
            // stream certifies clean. The trade is intentional — an aggregate
            // whose type this checker does not model gets envelope-level checks
            // only, and inventing a prior state for it is how the false red
            // happened — but it is a loosening, not a restoration.
            if seen_creation.contains(&agg_key)
                && let Some(current) = replay.get(&agg_key)
                && current.state != from
            {
                findings.push(at(
                    FindingCode::FromStateMismatch,
                    format!("payload.from is {from} but replay says {}", current.state),
                ));
            }
            match fsm_check(&agg_type, from, to) {
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
            // The blocked-flip rule: this flip demands the liveness producer and a receipt.
            if agg_type == "attempt"
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
            if agg_type == "command" {
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
                            // The creation-declared set is the FLOOR a `clean`
                            // claim must cover. The terminal event may RESTATE
                            // the targets but never NARROW them: union the two,
                            // red any declared target the terminal event
                            // dropped, and judge agreement over the full set.
                            let created: Vec<String> =
                                ctx.map(|c| c.targets.clone()).unwrap_or_default();
                            let restated = payload_targets(event);
                            if let Some(listed) = &restated {
                                for dropped in created.iter().filter(|t| !listed.contains(t)) {
                                    findings.push(at(
                                        FindingCode::OutcomeDisagreesWithTargets,
                                        format!(
                                            "outcome clean but declared target {dropped} was dropped from the terminal event's target list"
                                        ),
                                    ));
                                }
                            }
                            let mut targets = created;
                            if let Some(listed) = restated {
                                for t in listed {
                                    if !targets.contains(&t) {
                                        targets.push(t);
                                    }
                                }
                            }
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
                        // `partial` and `unknown` are legitimate outcomes with
                        // no target-agreement to judge.
                        Some("partial") | Some("unknown") => {}
                        // Any other string is not a real outcome — accepting it
                        // silently would skip the whole agreement check.
                        Some(other) => findings.push(at(
                            FindingCode::StateChangeMalformed,
                            format!("outcome {other} is not one of clean, partial, unknown"),
                        )),
                    }
                } else if outcome.is_some() {
                    findings.push(at(
                        FindingCode::OutcomeOnNonTerminal,
                        format!("outcome on a non-terminal transition to {to}"),
                    ));
                }
            }
            // The version cursor was advanced above; record only the resulting
            // state. The tracking entry always exists by now.
            if let Some(current) = replay.get_mut(&agg_key) {
                current.state = to.to_string();
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
