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

use common::{apply, drop_database, fresh_store, maintenance_pool, refuse};
use gwk_domain::command::KernelCommand;
use gwk_domain::ids::{PtySessionGeneration, PtySessionId};
use gwk_domain::protocol::KernelErrorCode;
use sqlx::Row;

fn open(id: &str, generation: &str) -> KernelCommand {
    KernelCommand::OpenPtySession {
        pty_session_id: PtySessionId::new(id),
        generation: PtySessionGeneration::new(generation),
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
async fn a_closed_session_refuses_reattach_and_reclose_but_records_a_late_detach() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "pty_closed", 8).await;

    apply(&store, "open", open("pty-1", "gen-1")).await;
    apply(&store, "att", attach("pty-1", 1)).await;
    apply(&store, "done", close("pty-1", 2)).await;

    let (code, msg) = refuse(&store, "att-after", attach("pty-1", 3)).await;
    assert_eq!(code, KernelErrorCode::Validation);
    assert!(msg.contains("already closed as closed"), "{msg}");
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
