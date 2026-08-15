//! Proves the driving-window figures against a real PostgreSQL.
//!
//! The queries in [`admin::driving_figures`] are the receipt's definition of
//! its figures, so the proof is a seeded ledger whose answers are computed by
//! hand — and whose shape was chosen so that every load-bearing clause gives a
//! DIFFERENT answer when mutated. The first cut of this seed could not tell
//! four definitions of "crashed generation" apart, because every candidate
//! read 1 against two generations with one open session each; the review that
//! found it mutated eleven clauses and watched five stay green. This seed is
//! the repair: three generations, unevenly closed, with the oldest and newest
//! sessions in different generations, a task event owning a day no pty event
//! covers, and a pty event before the window start.
//!
//! `#[ignore]` because it needs a server:
//!
//! ```text
//! docker run --rm -d -p 127.0.0.1:55432:5432 -e POSTGRES_HOST_AUTH_METHOD=trust \
//!   --name gwk-pg postgres:16
//! GWK_TEST_ADMIN_DATABASE_URL=postgres://postgres@localhost:55432/postgres \
//!   cargo test -p gwk-kernel --test receipt_figures -- --ignored
//! ```

mod common;

use common::{RUNTIME_ROLE, maintenance_pool, url_for};
use gwk_kernel::admin;
use gwk_kernel::config::{ADMIN_DATABASE_URL_ENV, AdminConfig, RUNTIME_ROLE_ENV};
use sqlx::PgPool;

/// A freshly created database with the contract applied, and a pool on it.
///
/// `maintenance_pool` handles the CREATE ROLE race (cases run concurrently;
/// the loser's "already exists" is the state it wanted).
async fn fresh_initialized(suffix: &str) -> PgPool {
    let maintenance = maintenance_pool().await;
    let name = format!("gwk_receipt_{}_{suffix}", std::process::id());
    common::drop_database(&maintenance, &name).await;
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

fn window(start: &str, end: &str) -> admin::ReceiptWindow {
    admin::ReceiptWindow {
        start: start.to_owned(),
        end: end.to_owned(),
    }
}

/// Nine sessions across three generations, five events across four days.
///
/// Directly by INSERT rather than through the port, on the same reasoning as
/// the perf harness's seeding: these rows are only ever read, and the figures
/// need `opened_at`/`closed_at` instants the append path would stamp with its
/// own clock.
///
/// Load-bearing shape, so a future edit knows what it is holding up:
/// - `s0` is the OLDEST session and g1 is fully closed — an oldest-session
///   crash subquery reads a different generation set than a newest-session
///   one.
/// - g2 holds TWO open sessions and one closed — `count(*)` disagrees with
///   `count(DISTINCT generation)`, and closed-rows disagree with open-rows.
/// - `s8` is the NEWEST session and g3 still holds an open one, so excluding
///   the newest generation excludes something real.
/// - `s6` overlaps 11:00, making the tie-instant peak (5) strictly higher
///   than any tie-free instant (4 on 08-03).
/// - `e5` (task) owns 08-03, a day NO pty event covers; `e0` (pty) sits
///   before the window start — each exists to fail one dropped filter.
async fn seed(pool: &PgPool) {
    sqlx::raw_sql(
        "INSERT INTO gwk.pty_session \
           (id, state, generation, attach_count, detach_count, opened_at, closed_at) VALUES \
         ('s0', 'closed',  'g1', 0, 0, '2026-08-01T08:00:00Z', '2026-08-01T08:30:00Z'), \
         ('s1', 'closed',  'g1', 2, 2, '2026-08-01T10:00:00Z', '2026-08-01T11:00:00Z'), \
         ('s2', 'closed',  'g1', 1, 1, '2026-08-01T10:30:00Z', '2026-08-01T12:00:00Z'), \
         ('s3', 'closed',  'g1', 0, 0, '2026-08-01T11:00:00Z', '2026-08-01T11:30:00Z'), \
         ('s6', 'closed',  'g2', 1, 1, '2026-08-01T10:45:00Z', '2026-08-01T11:10:00Z'), \
         ('s5', 'running', 'g2', 1, 0, '2026-08-01T09:00:00Z', NULL), \
         ('s4', 'running', 'g2', 3, 1, '2026-08-02T09:00:00Z', NULL), \
         ('s7', 'running', 'g3', 0, 0, '2026-08-03T10:00:00Z', NULL), \
         ('s8', 'closed',  'g3', 1, 1, '2026-08-03T11:00:00Z', '2026-08-03T11:20:00Z');",
    )
    .execute(pool)
    .await
    .expect("seed sessions");
    sqlx::raw_sql(
        "INSERT INTO gwk.event \
           (seq, event_id, project_id, aggregate_type, aggregate_id, aggregate_version, \
            event_type, schema_version, occurred_at, appended_at, actor, origin, payload) VALUES \
         (1, 'e0', 'p', 'pty_session', 's0', 1, 'opened',   1, '2026-07-30T12:00:00Z', now(), '{}', '{}', '{}'), \
         (2, 'e1', 'p', 'pty_session', 's1', 1, 'opened',   1, '2026-08-01T10:20:00Z', now(), '{}', '{}', '{}'), \
         (3, 'e2', 'p', 'pty_session', 's4', 1, 'opened',   1, '2026-08-02T09:05:00Z', now(), '{}', '{}', '{}'), \
         (4, 'e4', 'p', 'pty_session', 's4', 2, 'attached', 1, '2026-08-05T00:00:00Z', now(), '{}', '{}', '{}'), \
         (5, 'e5', 'p', 'task',        't1', 1, 'created',  1, '2026-08-03T10:00:00Z', now(), '{}', '{}', '{}');",
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
    let all = admin::driving_figures(
        &pool,
        &window("2026-08-01T00:00:00Z", "2026-08-04T00:00:00Z"),
    )
    .await
    .expect("figures");
    assert_eq!(all.sessions_in_window, 9);
    assert_eq!(all.sessions_alive_in_window, 9);
    // Two, not three or four: e5 is a TASK event and 08-03 has no pty event,
    // so counting it would invent a driving day (that is the mutation that
    // drops the aggregate_type filter); e0 is a pty event BEFORE the window
    // start, so a days query missing its lower bound reads 3.
    assert_eq!(all.days_driven, 2);
    // 5, uniquely at the tie instant: s3 opens the moment s1 closes while
    // s5, s2, and s6 are alive. With closes sorting first the same seed
    // reads 4 — from 08-03, a tie-free instant — so this assertion IS the
    // tie-break.
    assert_eq!(all.peak_concurrent, 5);
    assert_eq!(all.generations, 3);
    assert_eq!(all.restarts, 2);
    // One, and the seed makes every wrong definition read something else:
    // g2 alone left sessions running and was superseded (g3's own open
    // session does not count — g3 is the live generation). Counting ROWS
    // reads 2 (g2 holds two open sessions); anchoring on the OLDEST session
    // reads 2 (g1 is fully closed, so {g2, g3} survive); counting CLOSED
    // rows' generations reads 2 ({g1, g2}).
    assert_eq!(all.crashed_generations, 1);
    assert_eq!(all.attaches, 9);
    assert_eq!(all.detaches, 6);

    // A half-hour slice: two sessions open inside it (s6 exactly at the
    // inclusive end), four alive across it — the two denominators are
    // DIFFERENT sets, which is why both are reported.
    let edge = admin::driving_figures(
        &pool,
        &window("2026-08-01T10:15:00Z", "2026-08-01T10:45:00Z"),
    )
    .await
    .expect("figures");
    assert_eq!(edge.sessions_in_window, 2);
    assert_eq!(edge.sessions_alive_in_window, 4);
    assert_eq!(edge.days_driven, 1);
    assert_eq!(edge.peak_concurrent, 4);
    assert_eq!(edge.generations, 2);
    assert_eq!(edge.restarts, 1);
    // Whole-ledger by definition: narrowing the window must not hide a crash.
    assert_eq!(edge.crashed_generations, 1);
    // s1 and s5 opened before the slice and s6 closes after it; all are
    // alive in it with their FULL lifetime counters.
    assert_eq!(edge.attaches, 5);
    assert_eq!(edge.detaches, 4);

    // A window BEFORE every session: zeros beside zero denominators, and
    // peak exactly 0 — the close branch of the sweep requires its session
    // to have opened by the window end, and dropping that clause admits
    // unmatched closes that sum to a NEGATIVE peak here.
    let before = admin::driving_figures(
        &pool,
        &window("2026-07-01T00:00:00Z", "2026-07-15T00:00:00Z"),
    )
    .await
    .expect("figures");
    assert_eq!(before.sessions_in_window, 0);
    assert_eq!(before.sessions_alive_in_window, 0);
    assert_eq!(before.days_driven, 0);
    assert_eq!(before.peak_concurrent, 0);
    assert_eq!(before.generations, 0);
    assert_eq!(before.restarts, 0);
    assert_eq!(before.crashed_generations, 1);
    assert_eq!(before.attaches, 0);
    assert_eq!(before.detaches, 0);
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn an_empty_estate_reads_zero_not_minus_one() {
    let pool = fresh_initialized("empty").await;

    // No `to`: the default-to-now path, on the emptiest input there is.
    let window = admin::resolve_window(&pool, "2026-08-01T00:00:00Z", None)
        .await
        .expect("resolve");
    assert!(!window.end.is_empty());
    let figures = admin::driving_figures(&pool, &window)
        .await
        .expect("figures");
    assert_eq!(figures.sessions_in_window, 0);
    assert_eq!(figures.sessions_alive_in_window, 0);
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

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn the_window_resolves_once_and_refuses_inversion() {
    let pool = fresh_initialized("window").await;

    // A relative literal is accepted and leaves as the concrete instant it
    // meant — the resolved text, not the word, is what the receipt records
    // and what every figure query binds.
    let relative = admin::resolve_window(&pool, "yesterday", None)
        .await
        .expect("resolve a relative literal");
    assert!(!relative.start.contains("yesterday"), "{}", relative.start);

    // A transposed flag pair refuses instead of producing a receipt of
    // zeros byte-identical to a legitimately quiet window.
    let inverted =
        admin::resolve_window(&pool, "2026-09-01T00:00:00Z", Some("2026-01-01T00:00:00Z")).await;
    assert!(inverted.is_err(), "an inverted window must refuse");

    // A value that is not a timestamp at all refuses with the server's
    // message naming it.
    let nonsense = admin::resolve_window(&pool, "not-a-time", None).await;
    assert!(nonsense.is_err(), "a non-timestamp must refuse");
}
