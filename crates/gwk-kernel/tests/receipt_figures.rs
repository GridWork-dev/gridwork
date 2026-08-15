//! Proves the driving-window figures against a real PostgreSQL.
//!
//! The queries in [`admin::driving_figures`] are the receipt's definition of
//! its four figures, so the proof is a seeded ledger whose answers are
//! computed by hand — including the cases that discriminate: a session
//! opening the instant another closes, a session spanning the window edge,
//! and a window with nothing in it. `#[ignore]` because it needs a server:
//!
//! ```text
//! docker run --rm -d -p 127.0.0.1:55432:5432 -e POSTGRES_HOST_AUTH_METHOD=trust \
//!   --name gwk-pg postgres:16
//! GWK_TEST_ADMIN_DATABASE_URL=postgres://postgres@localhost:55432/postgres \
//!   cargo test -p gwk-kernel --test receipt_figures -- --ignored
//! ```

use gwk_kernel::admin;
use gwk_kernel::config::{ADMIN_DATABASE_URL_ENV, AdminConfig, RUNTIME_ROLE_ENV};
use sqlx::PgPool;

const ADMIN_URL_ENV: &str = "GWK_TEST_ADMIN_DATABASE_URL";
const RUNTIME_ROLE: &str = "gwk_test_runtime";

fn maintenance_url() -> String {
    std::env::var(ADMIN_URL_ENV).unwrap_or_else(|_| {
        panic!("{ADMIN_URL_ENV} must point at a PostgreSQL superuser DSN for this test")
    })
}

fn url_for(database: &str) -> String {
    let base = maintenance_url();
    let (prefix, _) = base
        .rsplit_once('/')
        .expect("a postgres URL carries a /database suffix");
    format!("{prefix}/{database}")
}

/// A freshly created database with the contract applied, and a pool on it.
async fn fresh_initialized(suffix: &str) -> PgPool {
    let maintenance = PgPool::connect(&maintenance_url())
        .await
        .expect("connect to the maintenance database");
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)")
            .bind(RUNTIME_ROLE)
            .fetch_one(&maintenance)
            .await
            .expect("query pg_roles");
    if !exists {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "CREATE ROLE {RUNTIME_ROLE} NOLOGIN;"
        )))
        .execute(&maintenance)
        .await
        .expect("create the runtime role");
    }

    let name = format!("gwk_receipt_{}_{suffix}", std::process::id());
    let _ = sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {name};"
    )))
    .execute(&maintenance)
    .await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name};")))
        .execute(&maintenance)
        .await
        .expect("create test database");

    let pool = PgPool::connect(&url_for(&name)).await.expect("connect");
    let url = url_for(&name);
    let config = AdminConfig::from_lookup(move |key| match key {
        ADMIN_DATABASE_URL_ENV => Some(url.clone()),
        RUNTIME_ROLE_ENV => Some(RUNTIME_ROLE.to_owned()),
        _ => None,
    })
    .expect("test config");
    admin::init(&pool, &config).await.expect("init");
    pool
}

/// Five sessions across two generations, five events across four days.
///
/// Directly by INSERT rather than through the port, on the same reasoning as
/// the perf harness's seeding: these rows are only ever read, and the figures
/// need `opened_at`/`closed_at` instants the append path would stamp with its
/// own clock.
async fn seed(pool: &PgPool) {
    sqlx::raw_sql(
        "INSERT INTO gwk.pty_session \
           (id, state, generation, attach_count, detach_count, opened_at, closed_at) VALUES \
         ('s5', 'running', 'g1', 1, 0, '2026-08-01T09:00:00Z', NULL), \
         ('s1', 'closed',  'g1', 2, 2, '2026-08-01T10:00:00Z', '2026-08-01T11:00:00Z'), \
         ('s2', 'closed',  'g1', 1, 1, '2026-08-01T10:30:00Z', '2026-08-01T12:00:00Z'), \
         ('s3', 'closed',  'g1', 0, 0, '2026-08-01T11:00:00Z', '2026-08-01T11:30:00Z'), \
         ('s4', 'running', 'g2', 3, 1, '2026-08-02T09:00:00Z', NULL);",
    )
    .execute(pool)
    .await
    .expect("seed sessions");
    sqlx::raw_sql(
        "INSERT INTO gwk.event \
           (seq, event_id, project_id, aggregate_type, aggregate_id, aggregate_version, \
            event_type, schema_version, occurred_at, appended_at, actor, origin, payload) VALUES \
         (1, 'e1', 'p', 'pty_session', 's1', 1, 'opened',   1, '2026-08-01T10:20:00Z', now(), '{}', '{}', '{}'), \
         (2, 'e2', 'p', 'pty_session', 's4', 1, 'opened',   1, '2026-08-02T09:05:00Z', now(), '{}', '{}', '{}'), \
         (3, 'e3', 'p', 'pty_session', 's4', 2, 'attached', 1, '2026-08-03T12:00:00Z', now(), '{}', '{}', '{}'), \
         (4, 'e4', 'p', 'pty_session', 's4', 3, 'attached', 1, '2026-08-05T00:00:00Z', now(), '{}', '{}', '{}'), \
         (5, 'e5', 'p', 'task',        't1', 1, 'created',  1, '2026-08-02T10:00:00Z', now(), '{}', '{}', '{}');",
    )
    .execute(pool)
    .await
    .expect("seed events");
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn the_figures_read_back_from_a_seeded_ledger() {
    let pool = fresh_initialized("seeded").await;
    seed(&pool).await;

    // The whole driving window.
    let all = admin::driving_figures(&pool, "2026-08-01T00:00:00Z", Some("2026-08-04T00:00:00Z"))
        .await
        .expect("figures");
    assert!(
        all.window_end.starts_with("2026-08-04"),
        "{}",
        all.window_end
    );
    assert_eq!(all.sessions_in_window, 5);
    // e4 is outside the window and e5 is not a pty_session event; both
    // counting would read 4 or a task-driven day as driving.
    assert_eq!(all.days_driven, 3);
    // 4, not 3: s3 opens at the instant s1 closes, and the sweep counts the
    // open before the close. This assertion IS the tie-break — with closes
    // sorting first the same seed reads 3.
    assert_eq!(all.peak_concurrent, 4);
    assert_eq!(all.generations, 2);
    assert_eq!(all.restarts, 1);
    // g1's s5 never closed and g2 superseded it; g2's own open session is
    // the live generation, not a crash.
    assert_eq!(all.crashed_generations, 1);
    assert_eq!(all.attaches, 7);
    assert_eq!(all.detaches, 4);

    // A half-hour slice: s1 and s5 opened before it and close (or never
    // close) after it, so they are alive in it with their FULL counters,
    // while only s2 was opened inside it.
    let edge = admin::driving_figures(&pool, "2026-08-01T10:15:00Z", Some("2026-08-01T10:45:00Z"))
        .await
        .expect("figures");
    assert_eq!(edge.sessions_in_window, 1);
    assert_eq!(edge.days_driven, 1);
    assert_eq!(edge.peak_concurrent, 3);
    assert_eq!(edge.generations, 1);
    assert_eq!(edge.restarts, 0);
    // Whole-ledger by definition: narrowing the window must not hide a crash.
    assert_eq!(edge.crashed_generations, 1);
    assert_eq!(edge.attaches, 4);
    assert_eq!(edge.detaches, 3);
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn an_empty_estate_reads_zero_not_minus_one() {
    let pool = fresh_initialized("empty").await;

    // No `to`: the default-to-now path, on the emptiest input there is.
    let figures = admin::driving_figures(&pool, "2026-08-01T00:00:00Z", None)
        .await
        .expect("figures");
    assert!(!figures.window_end.is_empty());
    assert_eq!(figures.sessions_in_window, 0);
    assert_eq!(figures.days_driven, 0);
    assert_eq!(figures.peak_concurrent, 0);
    assert_eq!(figures.generations, 0);
    // The figure the floor exists for: `count(DISTINCT) - 1` in SQL would
    // report -1 restarts for a window nothing drove.
    assert_eq!(figures.restarts, 0);
    // And the crash subquery's empty-table edge: no newest session means the
    // comparison is NULL, which must count as zero, not error.
    assert_eq!(figures.crashed_generations, 0);
    assert_eq!(figures.attaches, 0);
    assert_eq!(figures.detaches, 0);
}
