//! The pty session: one hosted lifetime as a kernel ledger object (P17).
//!
//! What the design is FOR: the S8 cutover receipt is read from the log, not
//! remembered — peak concurrency from open/close intervals, attach/detach
//! totals from the counters, host restarts from distinct generation tokens.
//! These cases certify the open→attach/detach→close lifecycle, the closed-
//! session refusals, the CAS, and the no-delete trigger proven by a seeded
//! DELETE straight at the table.
//!
//! `#[ignore]` because it needs a server — see `tests/common/mod.rs`.

mod common;

use common::{
    actor, apply, apply_as, drop_database, fresh_store, maintenance_pool, refuse, refuse_as,
};
use gwk_domain::command::KernelCommand;
use gwk_domain::ids::{
    AttemptId, EngineId, EngineSessionId, PtySessionGeneration, PtySessionId,
    PtySessionTemplateName, TaskId,
};
use gwk_domain::protocol::KernelErrorCode;
use sqlx::Row;

fn open(id: &str, generation: &str) -> KernelCommand {
    open_for_engine(id, generation, None)
}

fn open_for_engine(id: &str, generation: &str, engine_session_id: Option<&str>) -> KernelCommand {
    KernelCommand::OpenPtySession {
        pty_session_id: PtySessionId::new(id),
        generation: PtySessionGeneration::new(generation),
        engine_session_id: engine_session_id.map(EngineSessionId::new),
        title: None,
    }
}

fn attach(id: &str, expected_version: u32) -> KernelCommand {
    KernelCommand::RecordPtyAttach {
        pty_session_id: PtySessionId::new(id),
        expected_version,
    }
}

fn detach(id: &str, expected_version: u32) -> KernelCommand {
    KernelCommand::RecordPtyDetach {
        pty_session_id: PtySessionId::new(id),
        expected_version,
    }
}

fn close(id: &str, expected_version: u32) -> KernelCommand {
    KernelCommand::ClosePtySession {
        pty_session_id: PtySessionId::new(id),
        expected_version,
    }
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_session_counts_attaches_and_closes_terminal() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "pty_lifecycle", 8).await;

    apply(&store, "open", open("pty-1", "gen-1")).await;
    apply(&store, "att1", attach("pty-1", 1)).await;
    apply(&store, "det1", detach("pty-1", 2)).await;
    apply(&store, "att2", attach("pty-1", 3)).await;

    let row = sqlx::query(
        "SELECT state, generation, attach_count, detach_count, \
           closed_at IS NULL AS still_open \
         FROM gwk.pty_session WHERE id = 'pty-1'",
    )
    .fetch_one(store.pool())
    .await
    .expect("the live row");
    assert_eq!(row.get::<String, _>("state"), "running");
    assert_eq!(row.get::<String, _>("generation"), "gen-1");
    assert_eq!(row.get::<i64, _>("attach_count"), 2);
    assert_eq!(row.get::<i64, _>("detach_count"), 1);
    assert!(row.get::<bool, _>("still_open"));

    apply(&store, "done", close("pty-1", 4)).await;
    let row = sqlx::query(
        "SELECT state, closed_at IS NULL AS still_open \
         FROM gwk.pty_session WHERE id = 'pty-1'",
    )
    .fetch_one(store.pool())
    .await
    .expect("the closed row");
    assert_eq!(row.get::<String, _>("state"), "closed");
    assert!(
        !row.get::<bool, _>("still_open"),
        "close stamps the terminal instant"
    );

    // The closed row STAYS — the receipt reads history from it.
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM gwk.pty_session")
        .fetch_one(store.pool())
        .await
        .expect("count");
    assert_eq!(count, 1);

    drop(store);
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_stop_request_keeps_the_live_session_attachable_until_close() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "pty_stop_requested", 8).await;

    apply(&store, "open", open("pty-1:gen-1", "gen-1")).await;
    apply_as(
        &store,
        "stop",
        actor("operator"),
        KernelCommand::StopPtySession {
            pty_session_id: PtySessionId::new("pty-1"),
            generation: PtySessionGeneration::new("gen-1"),
        },
    )
    .await;
    apply(&store, "attach-after-stop", attach("pty-1:gen-1", 2)).await;

    let row = sqlx::query(
        "SELECT state, version, attach_count FROM gwk.pty_session WHERE id = 'pty-1:gen-1'",
    )
    .fetch_one(store.pool())
    .await
    .expect("live lifetime");
    assert_eq!(row.get::<String, _>("state"), "running");
    assert_eq!(row.get::<i64, _>("version"), 3);
    assert_eq!(row.get::<i64, _>("attach_count"), 1);

    drop(store);
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn successive_pty_lifetimes_join_from_child_to_engine_session() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "pty_engine_join", 8).await;

    apply(
        &store,
        "task",
        KernelCommand::CreateTask {
            task_id: TaskId::new("task-1"),
            kind: None,
            title: None,
            spec_ref: None,
            project: None,
            priority: None,
            tracker_ref: None,
        },
    )
    .await;
    apply(
        &store,
        "attempt",
        KernelCommand::CreateAttempt {
            attempt_id: AttemptId::new("attempt-1"),
            task_id: TaskId::new("task-1"),
            engine: EngineId::new("engine-1"),
            capability: None,
            role: None,
            model_lane: None,
            permission_profile: None,
            worktree_lease_id: None,
            base_sha: None,
            budget: None,
        },
    )
    .await;
    apply(
        &store,
        "engine-session",
        KernelCommand::OpenEngineSession {
            engine_session_id: EngineSessionId::new("session-1"),
            attempt_id: AttemptId::new("attempt-1"),
            engine: EngineId::new("engine-1"),
            provider_session_ref: None,
        },
    )
    .await;

    apply(
        &store,
        "pty-1",
        open_for_engine("pty-1", "gen-1", Some("session-1")),
    )
    .await;
    apply(
        &store,
        "pty-2",
        open_for_engine("pty-2", "gen-2", Some("session-1")),
    )
    .await;

    let rows: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT id, engine_session_id FROM gwk.pty_session ORDER BY id")
            .fetch_all(store.pool())
            .await
            .expect("pty engine joins");
    assert_eq!(
        rows,
        vec![
            ("pty-1".to_owned(), Some("session-1".to_owned())),
            ("pty-2".to_owned(), Some("session-1".to_owned())),
        ]
    );
    // The join is child-side, so opening PTYs must leave the parent alone.
    // `engine_session` carries no version column to read, and in an
    // event-sourced store the stronger statement is the log's anyway: the two
    // opens appended nothing to the parent aggregate. A row comparison would
    // pass against a rewrite that happened to land the same values.
    let parent_events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gwk.event \
         WHERE aggregate_type = 'engine_session' AND aggregate_id = 'session-1'",
    )
    .fetch_one(store.pool())
    .await
    .expect("parent engine session events");
    assert_eq!(
        parent_events, 1,
        "opening child PTYs must not rewrite their engine session"
    );

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_closed_session_refuses_reattach_and_reclose_but_records_a_late_detach() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "pty_closed", 8).await;

    apply(&store, "open", open("pty-1", "gen-1")).await;
    apply(&store, "att", attach("pty-1", 1)).await;
    apply(&store, "done", close("pty-1", 2)).await;

    let (code, msg) = refuse(&store, "att-after", attach("pty-1", 3)).await;
    assert_eq!(code, KernelErrorCode::Validation);
    assert!(msg.contains("closed and cannot be attached"), "{msg}");
    let (code, msg) = refuse(&store, "close-again", close("pty-1", 3)).await;
    assert_eq!(code, KernelErrorCode::Validation);
    assert!(msg.contains("already closed"), "{msg}");

    // A detach AFTER the close is true history: a retire drops the broadcast
    // sender and the attach streams end after the close lands. Refusing it
    // would systematically undercount the receipt's detach figure.
    apply(&store, "det-after", detach("pty-1", 3)).await;
    let row = sqlx::query(
        "SELECT state, attach_count, detach_count FROM gwk.pty_session WHERE id = 'pty-1'",
    )
    .fetch_one(store.pool())
    .await
    .expect("the closed row");
    assert_eq!(row.get::<String, _>("state"), "closed");
    assert_eq!(row.get::<i64, _>("attach_count"), 1);
    assert_eq!(row.get::<i64, _>("detach_count"), 1);

    drop(store);
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn stale_versions_bad_shapes_and_missing_sessions_are_refused() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "pty_shape", 8).await;

    apply(&store, "open", open("pty-1", "gen-1")).await;

    let (code, _) = refuse(&store, "stale", attach("pty-1", 7)).await;
    assert_eq!(code, KernelErrorCode::StaleVersion);
    let (code, msg) = refuse(&store, "missing", attach("pty-9", 1)).await;
    assert_eq!(code, KernelErrorCode::NotFound, "{msg}");

    // The shape refusal that never reaches the database.
    let (code, msg) = refuse(&store, "no-generation", open("pty-2", "  ")).await;
    assert_eq!(code, KernelErrorCode::Validation);
    assert!(msg.contains("host generation"), "{msg}");

    let (code, msg) = refuse_as(
        &store,
        "oversized-template",
        actor("operator"),
        KernelCommand::DeclarePtySessionTemplate {
            template_name: PtySessionTemplateName::new("oversized"),
            command: "/bin/cat".to_owned(),
            args: vec![],
            cwd: None,
            env: std::collections::BTreeMap::new(),
            cols: u16::MAX,
            rows: u16::MAX,
        },
    )
    .await;
    assert_eq!(code, KernelErrorCode::Validation);
    assert!(msg.contains("100000 cells"), "{msg}");

    let (code, msg) = refuse_as(
        &store,
        "oversized-resize",
        actor("operator"),
        KernelCommand::ResizePtySession {
            pty_session_id: PtySessionId::new("pty-1"),
            generation: PtySessionGeneration::new("gen-1"),
            cols: 1_000,
            rows: 101,
        },
    )
    .await;
    assert_eq!(code, KernelErrorCode::Validation);
    assert!(msg.contains("100000 cells"), "{msg}");

    drop(store);
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn the_seeded_delete_proves_the_row_is_ledger_history() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "pty_ledger", 8).await;

    apply(&store, "open", open("pty-1", "gen-1")).await;
    apply(&store, "done", close("pty-1", 1)).await;

    // Written past the kernel straight at the table: the trigger — not the
    // submit path — must be the refusal, or the receipt's promise is prose.
    let seeded = sqlx::query("DELETE FROM gwk.pty_session WHERE id = 'pty-1'")
        .execute(store.pool())
        .await;
    let err = seeded.expect_err("a session is ledger history").to_string();
    assert!(
        err.contains("pty_session"),
        "the no-delete trigger is the refusal: {err}"
    );

    // The half-closed shape the constraint pins down, seeded the same way.
    let seeded =
        sqlx::query("UPDATE gwk.pty_session SET state = 'running', version = 3 WHERE id = 'pty-1'")
            .execute(store.pool())
            .await;
    let err = seeded
        .expect_err("running with a closed_at is the pinned bug")
        .to_string();
    assert!(
        err.contains("pty_session_closed_iff_terminal"),
        "the CHECK is the refusal: {err}"
    );

    drop(store);
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn the_declared_template_catalog_is_cas_guarded_and_never_deleted() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "pty_template_ledger", 8).await;

    apply_as(
        &store,
        "declare-template",
        actor("operator"),
        KernelCommand::DeclarePtySessionTemplate {
            template_name: PtySessionTemplateName::new("review"),
            command: "/bin/cat".to_owned(),
            args: vec![],
            cwd: None,
            env: std::collections::BTreeMap::new(),
            cols: 100,
            rows: 30,
        },
    )
    .await;
    apply_as(
        &store,
        "retire-template",
        actor("operator"),
        KernelCommand::RetirePtySessionTemplate {
            template_name: PtySessionTemplateName::new("review"),
            expected_version: 1,
        },
    )
    .await;

    let wrong_cas =
        sqlx::query("UPDATE gwk.pty_session_template SET version = 4 WHERE name = 'review'")
            .execute(store.pool())
            .await
            .expect_err("a template version may advance by exactly one")
            .to_string();
    assert!(
        wrong_cas.contains("version must advance by exactly 1"),
        "the CAS trigger refused it: {wrong_cas}"
    );

    let deleted = sqlx::query("DELETE FROM gwk.pty_session_template WHERE name = 'review'")
        .execute(store.pool())
        .await
        .expect_err("template history cannot be deleted")
        .to_string();
    assert!(
        deleted.contains("pty_session_template"),
        "the no-delete trigger refused it: {deleted}"
    );

    let truncated = sqlx::query("TRUNCATE gwk.pty_session_template")
        .execute(store.pool())
        .await
        .expect_err("template history cannot be truncated")
        .to_string();
    assert!(
        truncated.contains("pty_session_template"),
        "the no-truncate trigger refused it: {truncated}"
    );

    let oversized = sqlx::query(
        "INSERT INTO gwk.pty_session_template (name, command, cols, rows) \
         VALUES ('oversized', '/bin/cat', 1000, 101)",
    )
    .execute(store.pool())
    .await
    .expect_err("the table pins the resident cell allocation bound")
    .to_string();
    assert!(
        oversized.contains("pty_template_grid_cell_bound"),
        "the cell-bound CHECK refused it: {oversized}"
    );

    drop(store);
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_declared_template_persists_environment_references_not_secret_values() {
    use gwk_domain::{KernelCommand, KernelResult, PtySessionTemplateName};
    use sqlx::Row;

    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "pty_template_env_refs", 8).await;
    let command = KernelCommand::DeclarePtySessionTemplate {
        template_name: PtySessionTemplateName::new("secret-safe"),
        command: "/bin/cat".to_owned(),
        args: vec![],
        cwd: None,
        env: std::collections::BTreeMap::from([(
            "TOKEN".to_owned(),
            "env:GWK_TEST_TEMPLATE_TOKEN".to_owned(),
        )]),
        cols: 80,
        rows: 24,
    };
    assert!(matches!(
        store
            .submit(&common::envelope_as(
                "declare-env-ref",
                actor("operator"),
                &command,
            ))
            .await,
        KernelResult::CommandApplied { .. }
    ));

    let row = sqlx::query("SELECT env FROM gwk.pty_session_template WHERE name = 'secret-safe'")
        .fetch_one(store.pool())
        .await
        .expect("template row");
    let env: serde_json::Value = row.get("env");
    assert_eq!(env["TOKEN"], "env:GWK_TEST_TEMPLATE_TOKEN");
    let payload: serde_json::Value = sqlx::query_scalar(
        "SELECT payload FROM gwk.event WHERE event_type = 'pty_session_template_declared'",
    )
    .fetch_one(store.pool())
    .await
    .expect("template event");
    assert_eq!(payload["env"]["TOKEN"], "env:GWK_TEST_TEMPLATE_TOKEN");

    drop_database(&maintenance, &name).await;
}
