//! Certifies the messaging, execution-command, gate, and evidence paths against
//! a real server.
//!
//! What only a real database can show: that the message and command FSM
//! triggers accept exactly the accepted edges, that
//! `outcome_iff_verification_complete` makes a split write unrepresentable,
//! that a message's uniqueness is the envelope key's, and that `byte_size`
//! survives the `numeric(20,0)` round trip at the top of `u64`.
//!
//! `#[ignore]` because it needs a server — see `tests/common/mod.rs`.

mod common;

use common::{
    apply, drop_database, envelope, event_count, fresh_store, maintenance_pool, refuse, state_row,
};
use gwk_domain::command::KernelCommand;
use gwk_domain::fsm::{CommandState, GateVerdict, MessageState, Outcome};
use gwk_domain::ids::{ByteCount, CommandId, EvidenceId, GateId, MessageId};
use gwk_domain::protocol::{KernelErrorCode, KernelResult};
use gwk_kernel::store::PgEventStore;
use sqlx::Row;

async fn message_row(store: &PgEventStore, id: &str) -> (String, i64) {
    state_row(
        store,
        "SELECT state, version FROM gwk.message WHERE id = $1",
        id,
    )
    .await
}

async fn command_row(store: &PgEventStore, id: &str) -> (String, i64) {
    state_row(
        store,
        "SELECT state, version FROM gwk.command WHERE id = $1",
        id,
    )
    .await
}

fn message(id: &str) -> KernelCommand {
    KernelCommand::SendMessage {
        message_id: MessageId::new(id),
        correlation_id: None,
        reply_to: None,
        sender: Some("orchestrator".into()),
        recipient: Some("engine-a".into()),
        channel: Some("dispatch".into()),
        kind: Some("brief".into()),
        payload: Some(serde_json::json!({ "goal": "ship the kernel" })),
        deadline: None,
    }
}

fn move_message(id: &str, to: MessageState, expected_version: u32) -> KernelCommand {
    KernelCommand::TransitionMessage {
        message_id: MessageId::new(id),
        to,
        expected_version,
        dead_letter_reason: None,
    }
}

fn issue(id: &str) -> KernelCommand {
    KernelCommand::IssueCommand {
        command_id: CommandId::new(id),
        kind: "stop_attempt".to_owned(),
        targets: vec!["a-1".to_owned(), "a-2".to_owned()],
        actor: None,
    }
}

fn move_command(id: &str, to: CommandState, expected_version: u32) -> KernelCommand {
    KernelCommand::TransitionCommand {
        command_id: CommandId::new(id),
        to,
        expected_version,
    }
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_message_walks_its_spine_with_the_row_version_tracking_the_log() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "message", 64).await;

    let events = apply(&store, "send", message("m-1")).await;
    assert_eq!(events[0].event_type, "message_sent");
    assert_eq!(message_row(&store, "m-1").await, ("accepted".to_owned(), 1));

    // The row's uniqueness IS the log's: `gwk.message.idempotency_key` is NOT
    // NULL, and the value stored is the envelope key that sent it.
    let key: String = sqlx::query_scalar("SELECT idempotency_key FROM gwk.message WHERE id = $1")
        .bind("m-1")
        .fetch_one(store.pool())
        .await
        .expect("message key");
    assert_eq!(key, "send");

    for (step, (to, version)) in [
        (MessageState::Delivered, 1),
        (MessageState::Acknowledged, 2),
        (MessageState::Applied, 3),
    ]
    .into_iter()
    .enumerate()
    {
        apply(
            &store,
            &format!("m-step-{step}"),
            move_message("m-1", to, version),
        )
        .await;
    }
    assert_eq!(message_row(&store, "m-1").await, ("applied".to_owned(), 4));

    // `applied` is terminal in the accepted edge table — the DDL trigger and
    // the domain guard agree, and the refusal is the domain's typed one.
    let (code, message_text) = refuse(
        &store,
        "m-after",
        move_message("m-1", MessageState::Rejected, 4),
    )
    .await;
    assert_eq!(code, KernelErrorCode::IllegalEdge, "{message_text}");
    assert_eq!(message_row(&store, "m-1").await, ("applied".to_owned(), 4));

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_dead_letter_reason_is_written_once_and_not_erased_after() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "deadletter", 64).await;

    apply(&store, "send", message("m-1")).await;
    apply(
        &store,
        "kill",
        KernelCommand::TransitionMessage {
            message_id: MessageId::new("m-1"),
            to: MessageState::DeadLetter,
            expected_version: 1,
            dead_letter_reason: Some("no route to engine-a".into()),
        },
    )
    .await;

    let reason: Option<String> =
        sqlx::query_scalar("SELECT dead_letter_reason FROM gwk.message WHERE id = $1")
            .bind("m-1")
            .fetch_one(store.pool())
            .await
            .expect("reason");
    assert_eq!(reason.as_deref(), Some("no route to engine-a"));
    assert_eq!(
        message_row(&store, "m-1").await,
        ("dead_letter".to_owned(), 2)
    );

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_commands_outcome_and_its_terminal_state_are_one_write() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "cmdoutcome", 64).await;

    apply(&store, "issue", issue("c-1")).await;
    assert_eq!(command_row(&store, "c-1").await, ("issued".to_owned(), 1));

    // The projection keeps the FIRST target; the rest stay in the event.
    let target: Option<String> = sqlx::query_scalar("SELECT target FROM gwk.command WHERE id = $1")
        .bind("c-1")
        .fetch_one(store.pool())
        .await
        .expect("target");
    assert_eq!(target.as_deref(), Some("a-1"));

    apply(
        &store,
        "target",
        move_command("c-1", CommandState::Targeted, 1),
    )
    .await;
    apply(
        &store,
        "signal",
        move_command("c-1", CommandState::Signaled, 2),
    )
    .await;

    // `outcome_iff_verification_complete` ties the terminal state to a value
    // this command does not carry, so naming that state here is refused as a
    // typed validation rather than raised as a constraint violation from
    // inside the projection.
    let (code, text) = refuse(
        &store,
        "shortcut",
        move_command("c-1", CommandState::VerificationComplete, 3),
    )
    .await;
    assert_eq!(code, KernelErrorCode::Validation, "{text}");
    assert!(text.contains("outcome"), "{text}");
    assert_eq!(command_row(&store, "c-1").await, ("signaled".to_owned(), 3));

    // Recording the outcome IS that transition.
    apply(
        &store,
        "verify",
        KernelCommand::RecordCommandOutcome {
            command_id: CommandId::new("c-1"),
            expected_version: 3,
            outcome: Outcome::Partial,
        },
    )
    .await;
    let row = sqlx::query("SELECT state, outcome, version FROM gwk.command WHERE id = $1")
        .bind("c-1")
        .fetch_one(store.pool())
        .await
        .expect("command row");
    assert_eq!(
        (
            row.get::<String, _>("state"),
            row.get::<Option<String>, _>("outcome"),
            row.get::<i64, _>("version"),
        ),
        (
            "verification_complete".to_owned(),
            Some("partial".to_owned()),
            4
        )
    );

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn an_outcome_before_the_signal_is_an_illegal_edge_not_a_completion() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "cmdearly", 64).await;

    apply(&store, "issue", issue("c-1")).await;
    // issued -> verification_complete is not an accepted edge. Without the
    // edge check this would complete a command nobody ever signaled, and the
    // CHECK constraint would have nothing to say about it — the row would be
    // perfectly well-formed and perfectly wrong.
    let (code, text) = refuse(
        &store,
        "early",
        KernelCommand::RecordCommandOutcome {
            command_id: CommandId::new("c-1"),
            expected_version: 1,
            outcome: Outcome::Clean,
        },
    )
    .await;
    assert_eq!(code, KernelErrorCode::IllegalEdge, "{text}");
    assert_eq!(command_row(&store, "c-1").await, ("issued".to_owned(), 1));
    assert_eq!(event_count(&store).await, 1);

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_gate_is_decided_by_verdict_under_a_version_cas() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "gate", 64).await;

    apply(
        &store,
        "open",
        KernelCommand::OpenGate {
            gate_id: GateId::new("g-1"),
            attempt_id: None,
            phase_ref: Some("4p-kernel".into()),
            kind: Some("review".into()),
        },
    )
    .await;
    assert_eq!(
        state_row(
            &store,
            "SELECT verdict, version FROM gwk.gate WHERE id = $1",
            "g-1"
        )
        .await,
        ("pending".to_owned(), 1)
    );

    // A stale CAS is refused with the number a retrier needs.
    let (code, text) = refuse(
        &store,
        "stale",
        KernelCommand::DecideGate {
            gate_id: GateId::new("g-1"),
            expected_version: 7,
            verdict: GateVerdict::Pass,
            evidence_ref: None,
        },
    )
    .await;
    assert_eq!(code, KernelErrorCode::StaleVersion, "{text}");

    apply(
        &store,
        "fail",
        KernelCommand::DecideGate {
            gate_id: GateId::new("g-1"),
            expected_version: 1,
            verdict: GateVerdict::Fail,
            evidence_ref: Some("ev-1".into()),
        },
    )
    .await;

    // A verdict is a value, not an edge: re-deciding is legal, and the
    // evidence reference the first decision carried is not erased by a second
    // that names none.
    apply(
        &store,
        "pass",
        KernelCommand::DecideGate {
            gate_id: GateId::new("g-1"),
            expected_version: 2,
            verdict: GateVerdict::Pass,
            evidence_ref: None,
        },
    )
    .await;
    let row = sqlx::query("SELECT verdict, evidence_ref, version FROM gwk.gate WHERE id = $1")
        .bind("g-1")
        .fetch_one(store.pool())
        .await
        .expect("gate row");
    assert_eq!(
        (
            row.get::<String, _>("verdict"),
            row.get::<Option<String>, _>("evidence_ref"),
            row.get::<i64, _>("version"),
        ),
        ("pass".to_owned(), Some("ev-1".to_owned()), 3)
    );

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn evidence_is_written_once_and_carries_the_full_u64_byte_range() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "evidence", 64).await;

    apply(
        &store,
        "ev",
        KernelCommand::RecordEvidence {
            evidence_id: EvidenceId::new("ev-1"),
            kind: "diff".to_owned(),
            r#ref: "blob://sha256-abc".to_owned(),
            digest: Some("sha256-abc".into()),
            // The column is numeric(20,0) precisely so the top of the range
            // survives; an i64 path would have overflowed here.
            byte_size: Some(ByteCount::new(u64::MAX)),
        },
    )
    .await;

    let size: String = sqlx::query_scalar("SELECT byte_size::text FROM gwk.evidence WHERE id = $1")
        .bind("ev-1")
        .fetch_one(store.pool())
        .await
        .expect("byte size");
    assert_eq!(size, u64::MAX.to_string());

    // Its own aggregate: the event advances a version the row does not store.
    let version: i64 = sqlx::query_scalar(
        "SELECT aggregate_version FROM gwk.event WHERE aggregate_type = 'evidence'",
    )
    .fetch_one(store.pool())
    .await
    .expect("aggregate version");
    assert_eq!(version, 1);

    // A second record under the same id is refused by the log's CAS, not by a
    // separate existence check that could drift from it.
    let duplicate = KernelCommand::RecordEvidence {
        evidence_id: EvidenceId::new("ev-1"),
        kind: "log".to_owned(),
        r#ref: "blob://sha256-def".to_owned(),
        digest: None,
        byte_size: None,
    };
    assert!(
        matches!(
            store.submit(&envelope("ev2", &duplicate)).await,
            KernelResult::Error {
                code: KernelErrorCode::StaleVersion,
                ..
            }
        ),
        "a duplicate evidence id must not land twice"
    );

    drop_database(&maintenance, &name).await;
}
