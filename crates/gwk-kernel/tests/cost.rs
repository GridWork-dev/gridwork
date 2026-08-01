//! The spend ledger: rows, not accumulator columns.
//!
//! What the design is FOR: a token report must never contend with the FSM CAS
//! of the attempt it describes — the accrue-after/check-before race is a
//! recorded defect class — so a cost entry is its own aggregate and the
//! attempt's version does not move when one lands. These cases certify that
//! property against a real database, plus the two guards the table carries:
//! an entry accrues to at least one subject, and a written entry never moves.
//!
//! `#[ignore]` because it needs a server — see `tests/common/mod.rs`.

mod common;

use common::{apply, drop_database, event_count, fresh_store, maintenance_pool, refuse};
use gwk_domain::command::KernelCommand;
use gwk_domain::ids::{AttemptId, CostEntryId, CostMicros, EngineId, TaskId, TokenCount};
use gwk_domain::protocol::KernelErrorCode;
use sqlx::Row;

fn task(id: &str) -> KernelCommand {
    KernelCommand::CreateTask {
        task_id: TaskId::new(id),
        kind: None,
        title: None,
        spec_ref: None,
        project: None,
        priority: None,
        tracker_ref: None,
    }
}

fn attempt(id: &str, task_id: &str) -> KernelCommand {
    KernelCommand::CreateAttempt {
        attempt_id: AttemptId::new(id),
        task_id: TaskId::new(task_id),
        engine: EngineId::new("engine-a"),
        capability: None,
        role: None,
        model_lane: None,
        permission_profile: None,
        worktree_lease_id: None,
        base_sha: None,
        budget: None,
    }
}

fn entry(id: &str, attempt: Option<&str>) -> KernelCommand {
    KernelCommand::RecordCostEntry {
        cost_entry_id: CostEntryId::new(id),
        attempt_id: attempt.map(AttemptId::new),
        engine_session_id: None,
        dispatch_node_id: None,
        engine: EngineId::new("engine-a"),
        model: Some("model-x".into()),
        // The columns are numeric(20,0) and canonicalize to text on the wire;
        // a u64::MAX that came back different would mean a JSON number leaked
        // into the path somewhere.
        input_tokens: Some(TokenCount::new(u64::MAX)),
        cached_input_tokens: Some(TokenCount::new(800)),
        cache_write_tokens: None,
        output_tokens: Some(TokenCount::new(300)),
        reasoning_tokens: None,
        cost_micros: Some(CostMicros::new(125_000)),
        cost_is_estimate: Some(true),
    }
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_cost_entry_lands_without_moving_the_attempt_it_describes() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "cost_lands", 8).await;

    apply(&store, "task", task("t-1")).await;
    apply(&store, "attempt", attempt("a-1", "t-1")).await;

    apply(&store, "cost-1", entry("ce-1", Some("a-1"))).await;

    let row = sqlx::query(
        "SELECT attempt_id, engine, model, input_tokens::text, cached_input_tokens::text, \
                output_tokens::text, cost_micros::text, cost_is_estimate \
         FROM gwk.cost_entry WHERE id = $1",
    )
    .bind("ce-1")
    .fetch_one(store.pool())
    .await
    .expect("cost entry row");
    assert_eq!(row.get::<Option<String>, _>(0).as_deref(), Some("a-1"));
    assert_eq!(row.get::<String, _>(1), "engine-a");
    assert_eq!(row.get::<Option<String>, _>(2).as_deref(), Some("model-x"));
    assert_eq!(
        row.get::<Option<String>, _>(3).as_deref(),
        Some("18446744073709551615")
    );
    assert_eq!(row.get::<Option<String>, _>(4).as_deref(), Some("800"));
    assert_eq!(row.get::<Option<String>, _>(5).as_deref(), Some("300"));
    assert_eq!(row.get::<Option<String>, _>(6).as_deref(), Some("125000"));
    assert_eq!(row.get::<Option<bool>, _>(7), Some(true));

    // The property the ledger exists for: the attempt's CAS version did not
    // move. A second entry proves the ledger accumulates as rows.
    let version: i64 = sqlx::query_scalar("SELECT version FROM gwk.attempt WHERE id = 'a-1'")
        .fetch_one(store.pool())
        .await
        .expect("attempt version");
    assert_eq!(version, 1);
    apply(&store, "cost-2", entry("ce-2", Some("a-1"))).await;
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM gwk.cost_entry")
        .fetch_one(store.pool())
        .await
        .expect("count entries");
    assert_eq!(rows, 2);

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_cost_entry_that_accrues_to_nothing_is_refused() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "cost_orphan", 8).await;

    let before = event_count(&store).await;
    let (code, message) = refuse(&store, "cost-orphan", entry("ce-none", None)).await;
    assert_eq!(code, KernelErrorCode::Validation);
    assert!(message.contains("accrues to nothing"), "{message}");
    // Refused at the boundary: nothing reached the log.
    assert_eq!(event_count(&store).await, before);

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn the_spend_ledger_is_append_only_like_the_receipt_beside_it() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "cost_immutable", 8).await;

    apply(&store, "task", task("t-1")).await;
    apply(&store, "attempt", attempt("a-1", "t-1")).await;
    apply(&store, "cost-1", entry("ce-1", Some("a-1"))).await;

    // A spend fact that could be edited would let a rollup disagree with the
    // event that produced it — the exact drift the ledger design forbids.
    for statement in [
        "UPDATE gwk.cost_entry SET cost_micros = 0",
        "DELETE FROM gwk.cost_entry",
        "TRUNCATE gwk.cost_entry",
    ] {
        let error = sqlx::raw_sql(sqlx::AssertSqlSafe(statement.to_owned()))
            .execute(store.pool())
            .await
            .expect_err("append-only guard missing");
        assert!(
            error.to_string().contains("append-only"),
            "{statement}: {error}"
        );
    }

    drop_database(&maintenance, &name).await;
}
