//! The workflow run: the 7-act choreography as a kernel ledger object (E13).
//!
//! What the design is FOR: a run is resumable and replayable — its current
//! step survives a crash in one row, and the log replays the same lifecycle.
//! The kernel holds NO act taxonomy: `step` is an open string because the act
//! vocabulary is template data (decision 17), so these cases certify shape
//! only — the open→advance→close lifecycle, the closed-run refusals, the CAS,
//! and the no-delete trigger proven by a seeded DELETE straight at the table.
//!
//! `#[ignore]` because it needs a server — see `tests/common/mod.rs`.

mod common;

use common::{apply, drop_database, fresh_store, maintenance_pool, refuse};
use gwk_domain::command::KernelCommand;
use gwk_domain::ids::WorkflowRunId;
use gwk_domain::protocol::KernelErrorCode;
use sqlx::Row;

fn open(id: &str, template_ref: &str) -> KernelCommand {
    KernelCommand::OpenWorkflowRun {
        workflow_run_id: WorkflowRunId::new(id),
        template_ref: template_ref.to_owned(),
        template_sha256: None,
        task_id: None,
        title: None,
    }
}

fn advance(id: &str, step: &str, expected_version: u32) -> KernelCommand {
    KernelCommand::AdvanceWorkflowRun {
        workflow_run_id: WorkflowRunId::new(id),
        step: step.to_owned(),
        expected_version,
    }
}

fn close(id: &str, outcome: &str, expected_version: u32) -> KernelCommand {
    KernelCommand::CloseWorkflowRun {
        workflow_run_id: WorkflowRunId::new(id),
        outcome: outcome.to_owned(),
        expected_version,
    }
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_run_advances_through_open_steps_and_closes_terminal() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wf_lifecycle", 8).await;

    apply(&store, "open", open("run-1", "seven-act@1")).await;

    // The step names here are template vocabulary the kernel has never heard
    // of — which is the decision-17 property under test.
    apply(&store, "a1", advance("run-1", "spec", 1)).await;
    apply(&store, "a2", advance("run-1", "the-strangest-act", 2)).await;

    let row = sqlx::query(
        "SELECT state, step, closed_at IS NULL AS still_open \
         FROM gwk.workflow_run WHERE id = 'run-1'",
    )
    .fetch_one(store.pool())
    .await
    .expect("the resume row");
    assert_eq!(row.get::<String, _>("state"), "running");
    assert_eq!(
        row.get::<Option<String>, _>("step").as_deref(),
        Some("the-strangest-act")
    );
    assert!(row.get::<bool, _>("still_open"));

    apply(&store, "done", close("run-1", "completed", 3)).await;
    let row = sqlx::query(
        "SELECT state, closed_at IS NULL AS still_open \
         FROM gwk.workflow_run WHERE id = 'run-1'",
    )
    .fetch_one(store.pool())
    .await
    .expect("the closed row");
    assert_eq!(row.get::<String, _>("state"), "completed");
    assert!(
        !row.get::<bool, _>("still_open"),
        "close stamps the terminal instant"
    );

    // The closed row STAYS — a run is ledger history, unlike a workspace node.
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM gwk.workflow_run")
        .fetch_one(store.pool())
        .await
        .expect("count");
    assert_eq!(count, 1);

    drop(store);
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_closed_run_refuses_every_further_verb() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wf_closed", 8).await;

    apply(&store, "open", open("run-1", "seven-act@1")).await;
    apply(&store, "fail", close("run-1", "failed", 1)).await;

    let (code, msg) = refuse(&store, "adv-after", advance("run-1", "spec", 2)).await;
    assert_eq!(code, KernelErrorCode::Validation);
    assert!(msg.contains("already closed as failed"), "{msg}");
    let (code, msg) = refuse(&store, "close-again", close("run-1", "completed", 2)).await;
    assert_eq!(code, KernelErrorCode::Validation);
    assert!(msg.contains("already closed"), "{msg}");

    drop(store);
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn stale_versions_bad_shapes_and_missing_runs_are_refused() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wf_shape", 8).await;

    apply(&store, "open", open("run-1", "seven-act@1")).await;

    let (code, _) = refuse(&store, "stale", advance("run-1", "spec", 7)).await;
    assert_eq!(code, KernelErrorCode::StaleVersion);
    let (code, msg) = refuse(&store, "missing", advance("run-9", "spec", 1)).await;
    assert_eq!(code, KernelErrorCode::NotFound, "{msg}");

    // Shape refusals that never reach the database.
    let (code, msg) = refuse(&store, "no-template", open("run-2", "  ")).await;
    assert_eq!(code, KernelErrorCode::Validation);
    assert!(msg.contains("template"), "{msg}");
    let (code, msg) = refuse(&store, "blank-step", advance("run-1", "  ", 1)).await;
    assert_eq!(code, KernelErrorCode::Validation);
    assert!(msg.contains("named step"), "{msg}");
    let (code, msg) = refuse(&store, "bad-outcome", close("run-1", "shrugged", 1)).await;
    assert_eq!(code, KernelErrorCode::Validation);
    assert!(msg.contains("completed, failed, or canceled"), "{msg}");
    let (code, msg) = refuse(
        &store,
        "bad-digest",
        KernelCommand::OpenWorkflowRun {
            workflow_run_id: WorkflowRunId::new("run-3"),
            template_ref: "seven-act@1".to_owned(),
            template_sha256: Some("UPPERCASE-and-short".to_owned()),
            task_id: None,
            title: None,
        },
    )
    .await;
    assert_eq!(code, KernelErrorCode::Validation);
    assert!(msg.contains("64 lowercase hex"), "{msg}");

    drop(store);
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn the_seeded_delete_proves_the_row_is_ledger_history() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wf_ledger", 8).await;

    apply(&store, "open", open("run-1", "seven-act@1")).await;
    apply(&store, "done", close("run-1", "canceled", 1)).await;

    // Written past the kernel straight at the table: the trigger — not the
    // submit path — must be the refusal, or the migration's promise is prose.
    let seeded = sqlx::query("DELETE FROM gwk.workflow_run WHERE id = 'run-1'")
        .execute(store.pool())
        .await;
    let err = seeded.expect_err("a run is ledger history").to_string();
    assert!(
        err.contains("workflow_run"),
        "the no-delete trigger is the refusal: {err}"
    );

    // The half-closed shape the constraint pins down, seeded the same way.
    let seeded = sqlx::query(
        "UPDATE gwk.workflow_run SET state = 'running', version = 3 WHERE id = 'run-1'",
    )
    .execute(store.pool())
    .await;
    let err = seeded
        .expect_err("running with a closed_at is the pinned bug")
        .to_string();
    assert!(
        err.contains("workflow_run_closed_iff_terminal"),
        "the CHECK is the refusal: {err}"
    );

    drop(store);
    drop_database(&maintenance, &name).await;
}
