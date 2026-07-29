//! Certifies the PostgreSQL [`EventStore`] against a real server.
//!
//! The first test runs `gwk-cert`'s conformance suite — the same checks the
//! in-memory reference store passes, which is what "conforming backend" means
//! here. The rest cover what a trait-generic suite cannot reach: commit-order
//! allocation under genuine concurrency, the numeric boundary against a real
//! `numeric(20,0)` column, epoch supersession, the advisory lock, the wake-up
//! channel, and the admission bound.
//!
//! These cases take a `raw_store` — no genesis, no activation — because the
//! port sits BELOW the epoch: they append through it directly rather than
//! through the command path, and their assertions are about exact sequences and
//! watermarks. An epoch's two events would shift every one of those numbers
//! while proving nothing about the store.
//!
//! `#[ignore]` because it needs a server — see `tests/common/mod.rs` for the
//! one-line `docker run` that provides one.

mod common;

use std::time::Duration;

use common::{drop_database, maintenance_pool, raw_store, secret, seed_events, url_for};
use gwk_cert::conformance;
use gwk_domain::port::{AppendError, EventStore};
use gwk_kernel::numeric::{from_numeric_text, to_numeric_text};
use gwk_kernel::store::{PgEventStore, connect_pool};
use gwk_kernel::writer::WriterLock;
use sqlx::postgres::PgListener;

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn the_postgres_store_passes_the_contract_conformance_suite() {
    let maintenance = maintenance_pool().await;
    // Each check gets a fresh store, exactly as `conformance::run_all` promises.
    // Listed one by one rather than through `run_all` because that factory is
    // synchronous and connecting is not — and naming them makes a failure say
    // which contract property broke.
    let mut databases = Vec::new();
    macro_rules! case {
        ($tag:literal, $check:path) => {{
            let (name, store) = raw_store(&maintenance, $tag, 64).await;
            $check(&store).await;
            databases.push(name);
        }};
    }
    case!(
        "commit_order",
        conformance::check_append_assigns_commit_order
    );
    case!("cas_conflict", conformance::check_expected_version_conflict);
    case!("cas_recovery", conformance::check_cas_refusal_and_recovery);
    case!("fencing", conformance::check_fencing);
    case!("cursor", conformance::check_cursor_recovery);
    case!("rebuild", conformance::check_deterministic_rebuild);
    case!("watermark", conformance::check_watermark);
    case!("read_limit", conformance::check_read_limit_is_clamped);

    for name in databases {
        drop_database(&maintenance, &name).await;
    }
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn sequences_are_allocated_in_commit_order_under_concurrency() {
    let maintenance = maintenance_pool().await;
    let (name, store) = raw_store(&maintenance, "concurrent", 64).await;

    // Distinct aggregates, so nothing is serialized by the CAS — only by the
    // writer row lock. If allocation were not held to commit, two of these
    // could interleave and produce a sequence a reader never sees.
    let store = std::sync::Arc::new(store);
    let mut tasks = tokio::task::JoinSet::new();
    for i in 0..24u32 {
        let store = store.clone();
        tasks.spawn(async move {
            store
                .append(
                    0,
                    None,
                    vec![conformance::fixture_event(&format!("agg-{i}"), 1)],
                )
                .await
                .expect("concurrent append")[0]
                .global_sequence
                .value()
        });
    }
    let mut assigned: Vec<u64> = Vec::new();
    while let Some(result) = tasks.join_next().await {
        assigned.push(result.expect("task"));
    }
    assigned.sort_unstable();
    let unique = {
        let mut u = assigned.clone();
        u.dedup();
        u
    };
    assert_eq!(
        unique.len(),
        24,
        "two appends were handed the same sequence"
    );

    // Reading back must produce exactly the committed set, ascending — no
    // sequence is visible that was never assigned, and none is missing.
    let read: Vec<u64> = store
        .read_from(None, usize::MAX)
        .await
        .expect("read")
        .iter()
        .map(|e| e.global_sequence.value())
        .collect();
    assert_eq!(read, assigned, "read order is not commit order");
    assert_eq!(
        store
            .watermark()
            .await
            .expect("watermark")
            .map(|s| s.value()),
        assigned.last().copied()
    );

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn two_writers_racing_one_aggregate_produce_one_winner_and_honest_losers() {
    let maintenance = maintenance_pool().await;
    let (name, store) = raw_store(&maintenance, "casrace", 64).await;
    let store = std::sync::Arc::new(store);

    // The SAME aggregate at the same expected version, genuinely simultaneously.
    // The conformance suite pins the refusal contract sequentially and says so;
    // what only a real runtime against a real row lock can show is that the race
    // has exactly one winner and that every loser is told the truth.
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..16 {
        let store = store.clone();
        tasks.spawn(async move {
            store
                .append(0, None, vec![conformance::fixture_event("agg", 1)])
                .await
        });
    }
    let mut winners = 0;
    let mut losers = 0;
    while let Some(result) = tasks.join_next().await {
        match result.expect("task") {
            Ok(events) => {
                winners += 1;
                assert_eq!(events.len(), 1);
            }
            Err(AppendError::VersionConflict { actual, expected }) => {
                losers += 1;
                // The version the loser must re-read from — not a bare "try
                // again", which is how a retry loop becomes an infinite one.
                assert_eq!(actual, 1, "the refusal reported a version that never was");
                assert_eq!(expected, 0);
            }
            Err(other) => panic!("a CAS race must not produce {other:?}"),
        }
    }
    assert_eq!(winners, 1, "the CAS admitted more than one writer");
    assert_eq!(losers, 15);

    // And the log holds exactly the winner's event: no loser left a row behind on
    // its way to being refused.
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM gwk.event")
        .fetch_one(store.pool())
        .await
        .expect("count");
    assert_eq!(count, 1);

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn a_writer_killed_mid_append_leaves_no_row_no_lock_and_no_burnt_sequence() {
    let maintenance = maintenance_pool().await;
    let (name, store) = raw_store(&maintenance, "kill9", 8).await;
    let store = std::sync::Arc::new(store);
    store
        .append(0, None, vec![conformance::fixture_event("agg", 1)])
        .await
        .expect("the incumbent writes");
    let watermark = store.watermark().await.expect("watermark");

    // A writer that gets exactly as far as a real append does and then dies: the
    // writer row locked, its event inserted, nothing committed. Raw SQL because
    // the port has no "stop here" — and what is under test is the state the
    // DATABASE is left in when a client disappears, which is identical whether
    // that client was SIGKILLed, unplugged, or panicked.
    let doomed = connect_pool(&secret(&name), 1).await.expect("connect");
    let mut dying = doomed.begin().await.expect("begin");
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *dying)
        .await
        .expect("pid");
    let claimed: String =
        sqlx::query_scalar("SELECT next_seq::text FROM gwk_internal.writer FOR UPDATE")
            .fetch_one(&mut *dying)
            .await
            .expect("lock the writer row");
    let claimed: u64 = claimed.parse().expect("a number");
    sqlx::query(
        "INSERT INTO gwk.event (seq, event_id, project_id, aggregate_type, aggregate_id, \
            aggregate_version, event_type, schema_version, occurred_at, appended_at, \
            actor, origin, payload) \
         VALUES ($1::numeric, 'evt-doomed', 'p', 'task', 'agg', 2, 'doomed_tick', 1, \
            now(), now(), '{}'::jsonb, '{}'::jsonb, '{}'::jsonb)",
    )
    .bind(claimed.to_string())
    .execute(&mut *dying)
    .await
    .expect("the doomed writer inserts");
    sqlx::query("UPDATE gwk_internal.writer SET next_seq = $1::numeric")
        .bind((claimed + 1).to_string())
        .execute(&mut *dying)
        .await
        .expect("the doomed writer allocates");

    // A real append, which can only be waiting on the row lock the corpse holds.
    let successor = tokio::spawn({
        let store = store.clone();
        async move {
            store
                .append(1, None, vec![conformance::fixture_event("agg", 2)])
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !successor.is_finished(),
        "the successor did not wait for the lock, so this proved nothing"
    );

    let killed: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
        .bind(pid)
        .fetch_one(store.pool())
        .await
        .expect("terminate the doomed writer");
    assert!(killed, "the doomed backend was already gone");

    let appended = successor
        .await
        .expect("join")
        .expect("the successor appends once the corpse lets go");

    // The number the corpse had taken. A rolled-back append gives its sequence
    // back, which is the claim `store.rs` makes about allocation and the reason
    // the column is not a BIGSERIAL — a burnt number here would be a permanent
    // hole in the log a reader has to be told to expect.
    assert_eq!(
        appended[0].global_sequence.value(),
        claimed,
        "the dead writer's sequence was burnt rather than returned"
    );
    // Its row is gone with it, and the successor's took the version it claimed.
    let doomed_rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gwk.event WHERE event_id = 'evt-doomed'")
            .fetch_one(store.pool())
            .await
            .expect("count");
    assert_eq!(doomed_rows, 0, "an uncommitted append survived a crash");
    assert!(store.watermark().await.expect("watermark") > watermark);

    drop(dying);
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn a_log_longer_than_a_page_still_reads_back_as_one_page() {
    let maintenance = maintenance_pool().await;
    let (name, store) = raw_store(&maintenance, "ceiling", 8).await;

    // A page and one. The check refuses to pass on fewer, because a shorter log
    // proves nothing about a ceiling.
    seed_events(store.pool(), gwk_domain::port::MAX_READ_LIMIT as u64 + 1).await;
    conformance::check_read_limit_ceiling(&store).await;

    // The clamp is the STORE's, not the caller's: this asks for everything and
    // the SQL is what says no. A `LIMIT` built from the request would return
    // 65,537 rows and, at this row width, a response no frame could carry.
    let asked_for_everything = store.read_from(None, usize::MAX).await.expect("read");
    assert_eq!(asked_for_everything.len(), gwk_domain::port::MAX_READ_LIMIT);

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn the_numeric_column_carries_the_full_u64_range() {
    let maintenance = maintenance_pool().await;
    // Straight through a real numeric(20,0), which is the only way to catch a
    // driver that quietly routes the value through f64 or i64.
    for value in [
        0u64,
        1,
        9_007_199_254_740_993, // above f64's exact range
        i64::MAX as u64,
        (i64::MAX as u64) + 1, // above bigint
        u64::MAX,
    ] {
        let back: String = sqlx::query_scalar("SELECT ($1::numeric(20,0))::text")
            .bind(to_numeric_text(value))
            .fetch_one(&maintenance)
            .await
            .unwrap_or_else(|e| panic!("round trip {value}: {e}"));
        assert_eq!(from_numeric_text(&back), Ok(value), "round trip of {value}");
    }
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn a_replayed_keyed_batch_returns_the_original_events() {
    let maintenance = maintenance_pool().await;
    let (name, store) = raw_store(&maintenance, "idempotent", 64).await;

    let mut event = conformance::fixture_event("agg", 1);
    event.idempotency_key = Some(gwk_domain::ids::IdempotencyKey::new("retry-me"));

    let first = store
        .append(0, None, vec![event.clone()])
        .await
        .expect("first append");
    // The retry presents the SAME expected_version it did the first time — the
    // CAS would call that a conflict, so recognizing the replay has to happen
    // before the version check runs.
    let replay = store
        .append(0, None, vec![event.clone()])
        .await
        .expect("a retried keyed batch must replay, not conflict");
    assert_eq!(
        first[0].global_sequence.value(),
        replay[0].global_sequence.value(),
        "a replay must return the ORIGINAL sequence, not a new one"
    );
    assert_eq!(first, replay);

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM gwk.event")
        .fetch_one(store.pool())
        .await
        .expect("count");
    assert_eq!(count, 1, "the replay wrote a second row");

    // A batch that only PARTLY matches is a caller bug, not a replay.
    let mut fresh = conformance::fixture_event("agg", 2);
    fresh.idempotency_key = Some(gwk_domain::ids::IdempotencyKey::new("retry-me"));
    let mut other = conformance::fixture_event("agg", 3);
    other.idempotency_key = Some(gwk_domain::ids::IdempotencyKey::new("never-seen"));
    let err = store
        .append(1, None, vec![fresh, other])
        .await
        .expect_err("a partial key match must not half-apply");
    assert!(
        matches!(err, AppendError::MalformedBatch(_)),
        "expected MalformedBatch, got {err:?}"
    );

    // A batch of the right SIZE under a key that already landed is still not a
    // replay unless it is the same request. Counting rows and calling it one
    // answers a different command with the original's events and reports it
    // applied — and this port has no pre-check in front of it to notice.
    let mut different = conformance::fixture_event("agg", 2);
    different.idempotency_key = Some(gwk_domain::ids::IdempotencyKey::new("retry-me"));
    different.event_type = "something-else".into();
    let err = store
        .append(1, None, vec![different])
        .await
        .expect_err("a different request under a used key must not replay");
    let AppendError::MalformedBatch(reason) = err else {
        panic!("expected MalformedBatch, got {err:?}");
    };
    assert!(reason.contains("identical batch"), "{reason}");

    // And the genuine retry still replays after all that.
    let again = store
        .append(0, None, vec![event])
        .await
        .expect("the original retry is still stable");
    assert_eq!(first, again);

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn a_superseded_epoch_cannot_commit() {
    let maintenance = maintenance_pool().await;
    let (name, first) = raw_store(&maintenance, "epoch", 64).await;

    first
        .append(0, None, vec![conformance::fixture_event("agg", 1)])
        .await
        .expect("the incumbent can write");

    // A second process boots against the same database and takes the epoch.
    let pool = connect_pool(&secret(&name), 4).await.expect("connect");
    let second = PgEventStore::open(pool).await.expect("second boot");
    assert!(second.boot_epoch() > first.boot_epoch());

    let err = first
        .append(1, None, vec![conformance::fixture_event("agg", 2)])
        .await
        .expect_err("the deposed process must not commit");
    let AppendError::Storage(reason) = err else {
        panic!("expected Storage, got {err:?}");
    };
    assert!(reason.contains("superseded"), "{reason}");

    // And the successor is unaffected.
    second
        .append(1, None, vec![conformance::fixture_event("agg", 2)])
        .await
        .expect("the successor writes");

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn only_one_process_may_hold_the_writer_lock() {
    let maintenance = maintenance_pool().await;
    let (name, _store) = raw_store(&maintenance, "lock", 64).await;

    let held = WriterLock::acquire(&secret(&name))
        .await
        .expect("first take");
    assert!(!held.is_cancelled());
    let refused = WriterLock::acquire(&secret(&name))
        .await
        .expect_err("a second holder must be refused, not queued");
    assert!(refused.to_string().contains("another kernel"), "{refused}");

    // Releasing makes it available again — the lock lives exactly as long as
    // its connection, which is what makes scope equal lifetime.
    drop(held);
    tokio::time::sleep(Duration::from_millis(250)).await;
    WriterLock::acquire(&secret(&name))
        .await
        .expect("available once released");

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn losing_the_lock_connection_cancels_the_writer() {
    let maintenance = maintenance_pool().await;
    let (name, _store) = raw_store(&maintenance, "lockloss", 64).await;

    let held = WriterLock::acquire_with_interval(&secret(&name), Duration::from_millis(100))
        .await
        .expect("take the lock");
    assert!(!held.is_cancelled(), "healthy at rest");

    // Kill the session holding the advisory lock from outside — the same thing
    // a network drop, a failover, or an admin does. PostgreSQL releases a
    // session advisory lock with its session, so at this instant this process
    // has silently stopped being the writer.
    let killed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM (SELECT pg_terminate_backend(l.pid) FROM pg_locks l \
         WHERE l.locktype = 'advisory' \
           AND l.database = (SELECT oid FROM pg_database WHERE datname = $1)) t",
    )
    .bind(&name)
    .fetch_one(&maintenance)
    .await
    .expect("terminate the lock holder");
    assert_eq!(killed, 1, "expected exactly one advisory-lock holder");

    tokio::time::timeout(Duration::from_secs(10), held.cancelled())
        .await
        .expect("losing the connection must cancel the writer");
    assert!(
        held.is_cancelled(),
        "the flag must latch, not just fire once"
    );

    // And the lock really is free: the successor takes it without waiting.
    WriterLock::acquire(&secret(&name))
        .await
        .expect("a successor can take the released lock");

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn a_commit_wakes_listeners_and_a_rollback_does_not() {
    let maintenance = maintenance_pool().await;
    let (name, store) = raw_store(&maintenance, "notify", 64).await;

    let mut listener = PgListener::connect(&url_for(&name))
        .await
        .expect("listener");
    listener
        .listen(gwk_kernel::EVENT_CHANNEL)
        .await
        .expect("listen");

    store
        .append(0, None, vec![conformance::fixture_event("agg", 1)])
        .await
        .expect("append");
    let notification = tokio::time::timeout(Duration::from_secs(5), listener.recv())
        .await
        .expect("a commit must wake a listener")
        .expect("notification");
    assert_eq!(
        notification.payload(),
        "1",
        "the wake-up carries the new watermark"
    );

    // A refused append must not announce anything: the NOTIFY is queued inside
    // the transaction, so PostgreSQL drops it when that transaction does.
    store
        .append(0, None, vec![conformance::fixture_event("agg", 1)])
        .await
        .expect_err("stale expected_version");
    let quiet = tokio::time::timeout(Duration::from_millis(500), listener.recv()).await;
    assert!(quiet.is_err(), "a rolled-back append announced itself");

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn a_saturated_writer_refuses_instead_of_queueing() {
    let maintenance = maintenance_pool().await;
    let (name, store) = raw_store(&maintenance, "bounded", 1).await;

    // Hold the writer row from outside, so the one admitted append blocks on
    // the lock and the bound is genuinely full.
    let mut blocker = store.pool().begin().await.expect("begin");
    sqlx::query("SELECT 1 FROM gwk_internal.writer WHERE id = 1 FOR UPDATE")
        .fetch_one(&mut *blocker)
        .await
        .expect("hold the writer row");

    let store = std::sync::Arc::new(store);
    let blocked = {
        let store = store.clone();
        tokio::spawn(async move {
            store
                .append(0, None, vec![conformance::fixture_event("agg-a", 1)])
                .await
        })
    };
    // Give the blocked append time to take the only permit and reach the lock.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let err = store
        .append(0, None, vec![conformance::fixture_event("agg-b", 1)])
        .await
        .expect_err("the bound must refuse once it is full");
    let AppendError::Storage(reason) = err else {
        panic!("expected Storage, got {err:?}");
    };
    assert!(reason.contains("queue is full"), "{reason}");

    // Releasing the row lets the queued one through, and the permit comes back.
    blocker.rollback().await.expect("release");
    blocked
        .await
        .expect("join")
        .expect("the blocked append lands");
    store
        .append(0, None, vec![conformance::fixture_event("agg-c", 1)])
        .await
        .expect("the bound recovers once the permit is returned");

    drop_database(&maintenance, &name).await;
}
