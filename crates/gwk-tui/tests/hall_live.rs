use gwk_domain::entity::{Attempt, AttentionItem, Message, Task};
use gwk_domain::envelope::{Actor, EventEnvelope, Origin};
use gwk_domain::fsm::{AttemptState, MessageState, TaskState};
use gwk_domain::ids::{
    AggregateId, AttemptId, AttentionItemId, EventId, IdempotencyKey, MessageId, ProjectId, Seq,
    TaskId, Timestamp,
};
use gwk_tui::estate::{EventIndex, ProjectionSnapshot, Stamped};
use gwk_tui::hall::AgentState;

fn timestamp() -> Timestamp {
    Timestamp::new("2026-08-08T12:00:00Z")
}

fn event(
    seq: u64,
    project: &str,
    aggregate_type: &str,
    aggregate_id: &str,
    aggregate_version: u32,
) -> EventEnvelope {
    EventEnvelope {
        event_id: EventId::new(format!("event-{seq}")),
        project_id: ProjectId::new(project),
        aggregate_type: aggregate_type.to_owned(),
        aggregate_id: AggregateId::new(aggregate_id),
        aggregate_version,
        event_type: if aggregate_type == "attention_item" {
            "attention_raised".to_owned()
        } else {
            format!("{aggregate_type}_changed")
        },
        schema_version: 1,
        global_sequence: Seq::new(seq),
        occurred_at: timestamp(),
        appended_at: timestamp(),
        actor: Actor {
            kind: "kernel".to_owned(),
            id: None,
        },
        origin: Origin {
            system: "kernel".to_owned(),
            r#ref: None,
        },
        causation_id: None,
        correlation_id: None,
        idempotency_key: None,
        payload: serde_json::json!({}),
        payload_ref: None,
    }
}

fn task() -> Task {
    Task {
        id: TaskId::new("task-execute"),
        version: 1,
        state: TaskState::Working,
        kind: Some("act:execute".to_owned()),
        title: Some("Execute".to_owned()),
        spec_ref: None,
        // The event remains the project-of-record even when this optional echo
        // is absent from a projection row.
        project: None,
        priority: None,
        tracker_ref: None,
        created_at: timestamp(),
        updated_at: timestamp(),
    }
}

fn attempt() -> Attempt {
    Attempt {
        id: AttemptId::new("attempt-1"),
        version: 2,
        state: AttemptState::Running,
        task_id: TaskId::new("task-execute"),
        engine: gwk_domain::ids::EngineId::new("codex"),
        capability: Some("code_write".to_owned()),
        role: Some("implementer".to_owned()),
        model_lane: None,
        permission_profile: None,
        worktree_lease_id: None,
        base_sha: None,
        budget: None,
        provider_session_ref: None,
        runtime_ref: Some("pid:42".to_owned()),
        runtime_started_at: Some(timestamp()),
        exit_code: None,
        provider_terminal_event: None,
        result_valid: None,
        evidence_manifest_ref: None,
        created_at: timestamp(),
        updated_at: timestamp(),
    }
}

fn message() -> Message {
    Message {
        id: MessageId::new("message-1"),
        version: 1,
        state: MessageState::Delivered,
        idempotency_key: IdempotencyKey::new("message-once"),
        correlation_id: None,
        reply_to: None,
        sender: Some("attempt-1".to_owned()),
        recipient: Some("operator".to_owned()),
        channel: Some("a2a".to_owned()),
        kind: Some("status".to_owned()),
        payload: None,
        deadline: None,
        delivery_attempts: 1,
        dead_letter_reason: None,
        delivery_refs: None,
        created_at: timestamp(),
        updated_at: timestamp(),
    }
}

fn attention() -> AttentionItem {
    AttentionItem {
        id: AttentionItemId::new("attention-1"),
        kind: "engine".to_owned(),
        summary: "operator input required".to_owned(),
        subject_ref: Some("attempt/attempt-1".to_owned()),
        raised_by: None,
        priority: Some(0),
        raised_at: timestamp(),
        acked_at: None,
        muted_until: None,
        resolved_at: None,
        resolution: None,
    }
}

#[test]
fn hall_live_uses_matching_event_sequences_never_projection_watermarks() {
    let mut events = EventIndex::default();
    events
        .ingest([
            event(7, "proj-alpha", "task", "task-execute", 1),
            event(11, "proj-alpha", "attempt", "attempt-1", 2),
            event(13, "proj-alpha", "message", "message-1", 1),
            event(17, "proj-alpha", "attention_item", "attention-1", 1),
        ])
        .expect("ordered event page");

    let page_watermark = Seq::new(900);
    let projections = ProjectionSnapshot {
        tasks: vec![Stamped::new(task(), page_watermark)],
        attempts: vec![Stamped::new(attempt(), page_watermark)],
        messages: vec![Stamped::new(message(), page_watermark)],
        attention: vec![Stamped::new(attention(), page_watermark)],
        watermarks: vec![Some(page_watermark)],
    };
    let estate = events.build(&projections).expect("projection/event join");

    assert_eq!(estate.frame.watermark, Some(page_watermark));
    assert_eq!(estate.frame.districts.len(), 1);
    let district = &estate.frame.districts[0];
    assert_eq!(district.id.as_str(), "district-r-proj-alpha");
    assert_eq!(district.changed_seq, Seq::new(17));
    assert_eq!(district.stations[0].id.as_str(), "station-r-act:execute");
    assert_eq!(district.stations[0].changed_seq, Seq::new(11));
    assert_eq!(district.stations[0].agents[0].changed_seq, Seq::new(11));
    assert_eq!(
        district.stations[0].agents[0].duration.as_deref(),
        Some("live")
    );
    assert_eq!(
        district.stations[0].agents[0].state,
        AgentState::NeedsAttention
    );
    assert_eq!(estate.frame.attention[0].changed_seq, Seq::new(17));
    assert_eq!(estate.messages[0].changed_seq, Seq::new(13));

    for seq in [
        district.changed_seq,
        district.stations[0].changed_seq,
        district.stations[0].agents[0].changed_seq,
        estate.frame.attention[0].changed_seq,
        estate.messages[0].changed_seq,
    ] {
        assert_ne!(
            seq, page_watermark,
            "a page watermark leaked into entity provenance"
        );
    }
}

#[test]
fn hall_live_refuses_a_projection_row_without_event_provenance() {
    let mut events = EventIndex::default();
    events
        .ingest([event(7, "proj-alpha", "task", "task-execute", 1)])
        .expect("task event");
    let projections = ProjectionSnapshot {
        tasks: vec![Stamped::new(task(), Seq::new(20))],
        attempts: vec![Stamped::new(attempt(), Seq::new(20))],
        messages: Vec::new(),
        attention: Vec::new(),
        watermarks: vec![Some(Seq::new(20))],
    };

    let error = events
        .build(&projections)
        .expect_err("the attempt has no matching event");
    assert!(error.to_string().contains("attempt-1"), "{error}");
    assert!(error.to_string().contains("sequence provenance"), "{error}");
}

#[test]
fn hall_live_refuses_projection_pages_that_straddle_an_append() {
    let projections = ProjectionSnapshot {
        watermarks: vec![Some(Seq::new(20)), Some(Seq::new(21))],
        ..ProjectionSnapshot::default()
    };
    let error = EventIndex::default()
        .build(&projections)
        .expect_err("mixed projection watermarks are not one frame");
    assert!(error.to_string().contains("straddled an append"), "{error}");
    assert!(EventIndex::is_retryable(&error));
}

#[test]
fn hall_live_refuses_attention_state_newer_than_its_page_watermark() {
    let mut events = EventIndex::default();
    events
        .ingest([event(17, "proj-alpha", "attention_item", "attention-1", 1)])
        .expect("attention event");
    let mut resolved = attention();
    resolved.resolved_at = Some(timestamp());
    let projections = ProjectionSnapshot {
        attention: vec![Stamped::new(resolved, Seq::new(17))],
        watermarks: vec![Some(Seq::new(17))],
        ..ProjectionSnapshot::default()
    };

    let error = events
        .build(&projections)
        .expect_err("resolved row cannot borrow the raise sequence");
    assert!(error.to_string().contains("unresolved=false"), "{error}");
    assert!(EventIndex::is_retryable(&error));
}

#[test]
fn hall_live_digest_normalizes_contract_valid_opaque_ids() {
    let project = "project with spaces/◆/and-a-name-that-is-longer-than-the-view-budget";
    let task_id = "task with spaces and ◆";
    let attempt_id = "attempt with spaces and ◆";
    let attention_id = "attention with spaces and ◆";
    let mut task = task();
    task.id = TaskId::new(task_id);
    task.kind = None;
    let mut attempt = attempt();
    attempt.id = AttemptId::new(attempt_id);
    attempt.task_id = TaskId::new(task_id);
    let mut attention = attention();
    attention.id = AttentionItemId::new(attention_id);
    attention.subject_ref = Some(format!("attempt/{attempt_id}"));

    let mut events = EventIndex::default();
    events
        .ingest([
            event(7, project, "task", task_id, 1),
            event(11, project, "attempt", attempt_id, 2),
            event(17, project, "attention_item", attention_id, 1),
        ])
        .expect("opaque-id events");
    let watermark = Seq::new(17);
    let projections = ProjectionSnapshot {
        tasks: vec![Stamped::new(task, watermark)],
        attempts: vec![Stamped::new(attempt, watermark)],
        attention: vec![Stamped::new(attention, watermark)],
        watermarks: vec![Some(watermark)],
        ..ProjectionSnapshot::default()
    };

    let first = events.build(&projections).expect("normalized estate");
    let second = events.build(&projections).expect("deterministic estate");
    assert_eq!(first.frame, second.frame);
    for id in [
        first.frame.districts[0].id.as_str(),
        first.frame.districts[0].stations[0].id.as_str(),
        first.frame.districts[0].stations[0].agents[0].id.as_str(),
        first.frame.attention[0].id.as_str(),
    ] {
        assert!(id.len() <= 64, "{id}");
        assert!(
            id.chars().all(|character| character.is_ascii_graphic()),
            "{id}"
        );
    }
}

#[test]
fn hall_live_raw_and_hashed_view_ids_cannot_alias() {
    use sha2::{Digest as _, Sha256};

    let opaque = "project with spaces";
    let digest = Sha256::digest(opaque.as_bytes());
    let alias = digest[..24]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let mut first_task = task();
    first_task.id = TaskId::new("task-first");
    let mut second_task = task();
    second_task.id = TaskId::new("task-second");
    let mut events = EventIndex::default();
    events
        .ingest([
            event(7, opaque, "task", "task-first", 1),
            event(8, &alias, "task", "task-second", 1),
        ])
        .expect("two project events");
    let watermark = Seq::new(8);
    let projections = ProjectionSnapshot {
        tasks: vec![
            Stamped::new(first_task, watermark),
            Stamped::new(second_task, watermark),
        ],
        watermarks: vec![Some(watermark)],
        ..ProjectionSnapshot::default()
    };

    let estate = events.build(&projections).expect("disjoint view ids");
    assert_eq!(estate.frame.districts.len(), 2);
    assert_ne!(estate.frame.districts[0].id, estate.frame.districts[1].id);
}

#[test]
fn hall_live_does_not_label_a_terminal_attempt_live() {
    let mut terminal = attempt();
    terminal.state = AttemptState::Succeeded;
    let mut events = EventIndex::default();
    events
        .ingest([
            event(7, "proj-alpha", "task", "task-execute", 1),
            event(11, "proj-alpha", "attempt", "attempt-1", 2),
        ])
        .expect("terminal attempt events");
    let watermark = Seq::new(11);
    let projections = ProjectionSnapshot {
        tasks: vec![Stamped::new(task(), watermark)],
        attempts: vec![Stamped::new(terminal, watermark)],
        watermarks: vec![Some(watermark)],
        ..ProjectionSnapshot::default()
    };
    let estate = events.build(&projections).expect("terminal estate");
    assert_eq!(
        estate.frame.districts[0].stations[0].agents[0].duration,
        None
    );
}
