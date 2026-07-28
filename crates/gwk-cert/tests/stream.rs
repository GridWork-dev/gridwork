//! The certifier against the synthetic golden stream — the 3′ vertical slice:
//! a full task/attempt/command story the checker certifies clean, and a
//! mutant battery where every single-field, single-edge, single-order, and
//! single-version corruption MUST be caught.

use gwk_cert::check::{FindingCode, check_stream, parse_stream};
use gwk_domain::envelope::{Actor, EventEnvelope, Origin};
use gwk_domain::ids::{
    AggregateId, CorrelationId, EventId, IdempotencyKey, ProjectId, Seq, Timestamp,
};

fn event(
    seq: u64,
    aggregate_type: &str,
    aggregate_id: &str,
    aggregate_version: u32,
    event_type: &str,
    actor_kind: &str,
    payload: serde_json::Value,
) -> EventEnvelope {
    EventEnvelope {
        event_id: EventId::new(format!("evt-{seq:04}")),
        project_id: ProjectId::new("proj-slice"),
        aggregate_type: aggregate_type.into(),
        aggregate_id: AggregateId::new(aggregate_id),
        aggregate_version,
        event_type: event_type.into(),
        schema_version: 1,
        global_sequence: Seq::new(seq),
        occurred_at: Timestamp::new("2026-07-27T12:00:00Z"),
        appended_at: Timestamp::new("2026-07-27T12:00:00Z"),
        actor: Actor {
            kind: actor_kind.into(),
            id: Some(format!("{actor_kind}-1")),
        },
        origin: Origin {
            system: "kernel".into(),
            r#ref: None,
        },
        causation_id: None,
        correlation_id: Some(CorrelationId::new("slice-1")),
        idempotency_key: Some(IdempotencyKey::new(format!("key-{seq}"))),
        payload,
        payload_ref: None,
    }
}

fn change(from: &str, to: &str) -> serde_json::Value {
    serde_json::json!({ "from": from, "to": to })
}

/// The valid golden story: task submitted->working, attempt runs, gets
/// blocked and unblocked by the liveness producer (receipted), a stop command
/// walks its spine, the attempt cancels, and the outcome is clean.
/// Sequences are Fibonacci — gaps are LEGAL, order is what matters.
fn valid_stream() -> Vec<EventEnvelope> {
    vec![
        event(
            1,
            "task",
            "task-1",
            1,
            "task_created",
            "operator",
            serde_json::json!({}),
        ),
        event(
            2,
            "task",
            "task-1",
            2,
            "task_state_changed",
            "kernel",
            change("submitted", "working"),
        ),
        event(
            3,
            "attempt",
            "att-1",
            1,
            "attempt_created",
            "kernel",
            serde_json::json!({}),
        ),
        event(
            5,
            "attempt",
            "att-1",
            2,
            "attempt_state_changed",
            "kernel",
            change("queued", "leased"),
        ),
        event(
            8,
            "attempt",
            "att-1",
            3,
            "attempt_state_changed",
            "kernel",
            change("leased", "starting"),
        ),
        event(
            13,
            "attempt",
            "att-1",
            4,
            "attempt_state_changed",
            "kernel",
            change("starting", "running"),
        ),
        event(
            21,
            "attempt",
            "att-1",
            5,
            "attempt_state_changed",
            "liveness_producer",
            {
                let mut v = change("running", "blocked");
                v["receipt_id"] = serde_json::json!("r-1");
                v
            },
        ),
        event(
            34,
            "receipt",
            "r-1",
            1,
            "receipt_recorded",
            "liveness_producer",
            serde_json::json!({
                "action": "state_flip", "subject_type": "attempt", "subject_id": "att-1",
                "from": "running", "to": "blocked", "observed_basis": "no heartbeat 120s"
            }),
        ),
        event(
            55,
            "attempt",
            "att-1",
            6,
            "attempt_state_changed",
            "liveness_producer",
            {
                let mut v = change("blocked", "running");
                v["receipt_id"] = serde_json::json!("r-2");
                v
            },
        ),
        event(
            89,
            "command",
            "cmd-1",
            1,
            "command_created",
            "operator",
            serde_json::json!({
                "kind": "stop_attempt", "targets": ["att-1"]
            }),
        ),
        event(
            144,
            "command",
            "cmd-1",
            2,
            "command_state_changed",
            "kernel",
            change("issued", "targeted"),
        ),
        event(
            233,
            "command",
            "cmd-1",
            3,
            "command_state_changed",
            "kernel",
            change("targeted", "signaled"),
        ),
        event(
            377,
            "attempt",
            "att-1",
            7,
            "attempt_state_changed",
            "kernel",
            change("running", "canceling"),
        ),
        event(
            610,
            "attempt",
            "att-1",
            8,
            "attempt_state_changed",
            "kernel",
            change("canceling", "canceled"),
        ),
        event(
            987,
            "task",
            "task-1",
            3,
            "task_state_changed",
            "kernel",
            change("working", "canceled"),
        ),
        event(
            1597,
            "command",
            "cmd-1",
            4,
            "command_state_changed",
            "kernel",
            serde_json::json!({
                "from": "signaled", "to": "verification_complete", "outcome": "clean", "targets": ["att-1"]
            }),
        ),
    ]
}

fn codes(events: &[EventEnvelope]) -> Vec<FindingCode> {
    check_stream(events).into_iter().map(|f| f.code).collect()
}

#[test]
fn the_valid_golden_stream_certifies_clean() {
    assert_eq!(check_stream(&valid_stream()), vec![]);
}

#[test]
fn the_committed_fixture_certifies_clean() {
    let raw = include_str!("../fixtures/valid-stream.json");
    let (events, parse_findings) = parse_stream(raw);
    assert_eq!(parse_findings, vec![]);
    assert_eq!(events.len(), 16);
    assert_eq!(check_stream(&events), vec![]);
}

/// Regenerate the committed fixture:
/// `cargo test -p gwk-cert regen_fixture -- --ignored`
#[test]
#[ignore = "writes the committed fixture; run explicitly to regenerate"]
fn regen_fixture() {
    let json = serde_json::to_string_pretty(&valid_stream()).expect("serialize");
    std::fs::write(
        concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/valid-stream.json"),
        format!("{json}\n"),
    )
    .expect("write fixture");
}

// ============================================================
// Mutant battery — every corruption class MUST be caught
// ============================================================

#[test]
fn mutant_one_field_schema_version() {
    let mut events = valid_stream();
    events[1].schema_version = 9;
    assert!(codes(&events).contains(&FindingCode::SchemaVersionUnknown));
}

#[test]
fn mutant_one_order_swap() {
    let mut events = valid_stream();
    events.swap(3, 4);
    assert!(codes(&events).contains(&FindingCode::SeqNotIncreasing));
}

#[test]
fn mutant_one_edge_illegal_transition() {
    let mut events = valid_stream();
    events[3].payload = change("queued", "running");
    assert!(codes(&events).contains(&FindingCode::IllegalTransition));
}

#[test]
fn mutant_one_version_gap() {
    let mut events = valid_stream();
    events[13].aggregate_version = 9;
    assert!(codes(&events).contains(&FindingCode::AggregateVersionGap));
}

#[test]
fn interleaved_non_state_event_keeps_the_version_cursor_in_sync() {
    let mut events = valid_stream();
    // A non-state event between two state changes consumes a version like any
    // other append; the stream stays contiguous and must certify clean.
    events.insert(
        14,
        event(
            700,
            "task",
            "task-1",
            3,
            "task_note_added",
            "operator",
            serde_json::json!({ "note": "checkpoint" }),
        ),
    );
    events[15].aggregate_version = 4; // working->canceled now sits at v4
    assert_eq!(check_stream(&events), vec![]);
}

#[test]
fn mutant_reused_aggregate_version() {
    let mut events = valid_stream();
    // A non-state event takes version 2, then the state change REUSES 2 —
    // unrepresentable in a store with a unique version constraint.
    events.insert(
        3,
        event(
            4,
            "attempt",
            "att-1",
            2,
            "attempt_output_appended",
            "kernel",
            serde_json::json!({ "chunk": "x" }),
        ),
    );
    assert!(codes(&events).contains(&FindingCode::AggregateVersionGap));
}

#[test]
fn mutant_flip_by_wrong_actor() {
    let mut events = valid_stream();
    events[6].actor.kind = "engine".into();
    assert!(codes(&events).contains(&FindingCode::FlipWrongActor));
}

#[test]
fn mutant_flip_without_receipt() {
    let mut events = valid_stream();
    events[6].payload = change("running", "blocked");
    assert!(codes(&events).contains(&FindingCode::FlipMissingReceipt));
}

#[test]
fn mutant_terminal_without_outcome() {
    let mut events = valid_stream();
    events[15].payload = change("signaled", "verification_complete");
    assert!(codes(&events).contains(&FindingCode::OutcomeMissing));
}

#[test]
fn mutant_outcome_on_non_terminal() {
    let mut events = valid_stream();
    events[11].payload["outcome"] = serde_json::json!("clean");
    assert!(codes(&events).contains(&FindingCode::OutcomeOnNonTerminal));
}

#[test]
fn terminal_without_targets_falls_back_to_the_created_ones() {
    let mut events = valid_stream();
    // The terminal event drops its `targets`; the ones named at creation
    // still bind the agreement check, and the golden stop stays clean.
    events[15].payload = serde_json::json!({
        "from": "signaled", "to": "verification_complete", "outcome": "clean"
    });
    assert_eq!(check_stream(&events), vec![]);
}

#[test]
fn mutant_clean_outcome_with_no_targets_anywhere() {
    let mut events = valid_stream();
    events[9].payload = serde_json::json!({ "kind": "stop_attempt" });
    events[15].payload = serde_json::json!({
        "from": "signaled", "to": "verification_complete", "outcome": "clean"
    });
    assert!(codes(&events).contains(&FindingCode::OutcomeDisagreesWithTargets));
}

#[test]
fn mutant_clean_outcome_with_unstopped_target() {
    let mut events = valid_stream();
    // The attempt ends `unknown` instead of `canceled`; `clean` now lies.
    events[13].payload = change("canceling", "unknown");
    assert!(codes(&events).contains(&FindingCode::OutcomeDisagreesWithTargets));
}

#[test]
fn mutant_clean_when_the_target_was_canceled_before_the_command_existed() {
    // The attempt cancels on its own; only THEN is the stop command issued
    // and walked to a clean claim. The target is `canceled` at the terminal
    // event, but this command stopped nothing.
    let events = vec![
        event(
            1,
            "attempt",
            "att-1",
            1,
            "attempt_created",
            "kernel",
            serde_json::json!({}),
        ),
        event(
            2,
            "attempt",
            "att-1",
            2,
            "attempt_state_changed",
            "kernel",
            change("queued", "leased"),
        ),
        event(
            3,
            "attempt",
            "att-1",
            3,
            "attempt_state_changed",
            "kernel",
            change("leased", "starting"),
        ),
        event(
            4,
            "attempt",
            "att-1",
            4,
            "attempt_state_changed",
            "kernel",
            change("starting", "running"),
        ),
        event(
            5,
            "attempt",
            "att-1",
            5,
            "attempt_state_changed",
            "kernel",
            change("running", "canceling"),
        ),
        event(
            6,
            "attempt",
            "att-1",
            6,
            "attempt_state_changed",
            "kernel",
            change("canceling", "canceled"),
        ),
        event(
            7,
            "command",
            "cmd-1",
            1,
            "command_created",
            "operator",
            serde_json::json!({ "kind": "stop_attempt", "targets": ["att-1"] }),
        ),
        event(
            8,
            "command",
            "cmd-1",
            2,
            "command_state_changed",
            "kernel",
            change("issued", "targeted"),
        ),
        event(
            9,
            "command",
            "cmd-1",
            3,
            "command_state_changed",
            "kernel",
            change("targeted", "signaled"),
        ),
        event(
            10,
            "command",
            "cmd-1",
            4,
            "command_state_changed",
            "kernel",
            serde_json::json!({
                "from": "signaled", "to": "verification_complete",
                "outcome": "clean", "targets": ["att-1"]
            }),
        ),
    ];
    assert!(codes(&events).contains(&FindingCode::OutcomeDisagreesWithTargets));
}

#[test]
fn mutant_clean_claimed_before_the_cancel_completed() {
    let mut events = valid_stream();
    // The cancel completes only AFTER verification claimed clean; as of the
    // terminal event the target was still `canceling`.
    let mut cancel = events.remove(13);
    cancel.global_sequence = Seq::new(2000);
    events.push(cancel);
    assert!(codes(&events).contains(&FindingCode::OutcomeDisagreesWithTargets));
}

#[test]
fn mutant_duplicate_idempotency_key() {
    let mut events = valid_stream();
    events[4].idempotency_key = events[3].idempotency_key.clone();
    assert!(codes(&events).contains(&FindingCode::IdempotencyDuplicate));
}

#[test]
fn mutant_transition_out_of_a_terminal() {
    let mut events = valid_stream();
    events.push(event(
        2000,
        "task",
        "task-1",
        4,
        "task_state_changed",
        "kernel",
        change("canceled", "working"),
    ));
    assert!(codes(&events).contains(&FindingCode::TerminalMutation));
}

#[test]
fn mutant_from_state_disagrees_with_replay() {
    let mut events = valid_stream();
    events[4].payload = change("queued", "starting");
    let found = codes(&events);
    assert!(found.contains(&FindingCode::FromStateMismatch));
    assert!(found.contains(&FindingCode::IllegalTransition));
}

#[test]
fn mutant_oversized_inline_payload() {
    let mut events = valid_stream();
    events[0].payload = serde_json::json!({ "blob": "x".repeat(70 * 1024) });
    assert!(codes(&events).contains(&FindingCode::InlinePayloadTooLarge));
}

#[test]
fn mutant_payload_ref_without_scheme() {
    let mut events = valid_stream();
    events[0].payload_ref = Some(gwk_domain::envelope::PayloadRef {
        digest: "deadbeef".into(),
        media_type: "application/octet-stream".into(),
        byte_size: gwk_domain::ids::ByteCount::new(1),
        retention_class: None,
        evidence_pin: None,
    });
    assert!(codes(&events).contains(&FindingCode::PayloadRefInvalid));
}

#[test]
fn mutant_second_creation() {
    let mut events = valid_stream();
    let dup = event(
        2000,
        "task",
        "task-1",
        4,
        "task_created",
        "operator",
        serde_json::json!({}),
    );
    events.push(dup);
    assert!(codes(&events).contains(&FindingCode::CreatedNotFirst));
}

#[test]
fn mutant_state_change_on_an_uncreated_aggregate() {
    let mut events = valid_stream();
    // An aggregate fabricated mid-stream, straight into a terminal — the
    // guarded ladder never ran.
    events.push(event(
        2000,
        "attempt",
        "att-ghost",
        1,
        "attempt_state_changed",
        "kernel",
        change("running", "succeeded"),
    ));
    assert!(codes(&events).contains(&FindingCode::CreationMissing));
}

#[test]
fn mutant_created_in_a_non_initial_state() {
    let mut events = valid_stream();
    // Born terminal: no transition ever runs, the whole ladder is skipped.
    events[0].payload = serde_json::json!({ "state": "completed" });
    assert!(codes(&events).contains(&FindingCode::CreationStateInvalid));
}

#[test]
fn malformed_envelope_becomes_a_finding_not_an_abort() {
    let (events, findings) = parse_stream(r#"[{ "not": "an envelope" }]"#);
    assert!(events.is_empty());
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].code, FindingCode::EnvelopeMalformed);
}
