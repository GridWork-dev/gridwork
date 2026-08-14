//! Certifies projection snapshots: the canonical form, the barrier that
//! triggers one, and the reference that keeps its records alive.
//!
//! What only a real database can show: that every projection table round-trips
//! through its CONTRACT type — the parity check between the DDL and
//! `gwk-domain`, which nothing else in the suite performs — that a 64-bit
//! counter survives the trip as a decimal string rather than losing precision
//! as a JSON number, that the hash is stable across reads and is the records
//! blob's own address, that both bounds of the barrier fire, and that sweep
//! will not reclaim a live checkpoint's records.
//!
//! `#[ignore]` because it needs a server — see `tests/common/mod.rs`.

mod common;

use std::sync::Arc;

use common::{
    TEST_KEK, TEST_REVISION, apply, blob_store_with, checkpointing_store, drop_database,
    fresh_store, maintenance_pool, populate, read_all, runtime_dir, secret, task,
};
use gwk_domain::blob::BlobAddress;
use gwk_domain::port::{BlobStore, EventStore};
use gwk_kernel::checkpoint::{self, RECORDS_MEDIA_TYPE};
use gwk_kernel::recover::Verdict;
use gwk_kernel::store::{PgEventStore, connect_pool};
use gwk_kernel::wire::listen::Listener;
use gwk_kernel::wire::serve::{self, Daemon};

/// The `projection_type` tags present in a canonical dump.
fn tags(records: &[u8]) -> Vec<String> {
    let text = String::from_utf8(records.to_vec()).expect("canonical records are utf-8");
    text.lines()
        .map(|line| {
            let value: serde_json::Value = serde_json::from_str(line).expect("a record per line");
            value["projection_type"]
                .as_str()
                .expect("every record is tagged")
                .to_owned()
        })
        .collect()
}

async fn canonical(store: &PgEventStore) -> Vec<u8> {
    let mut conn = store.pool().acquire().await.expect("connection");
    checkpoint::canonical_records(&mut conn)
        .await
        .expect("canonical records")
}

/// The subset a checkpoint hashes: every table a replay can rebuild.
async fn derived(store: &PgEventStore) -> Vec<u8> {
    let mut conn = store.pool().acquire().await.expect("connection");
    checkpoint::derived_records(&mut conn)
        .await
        .expect("derived records")
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn every_projection_round_trips_through_its_contract_type() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "cpparity", 8).await;
    populate(&store).await;

    // The parity check. Every entity is `deny_unknown_fields`, so a column the
    // contract type has no field for fails HERE — which is the point: the
    // alternative is a column that quietly never reaches the hash, and a
    // checkpoint that validates while describing less than the state.
    let records = canonical(&store).await;
    let present = tags(&records);
    for expected in [
        "attempt",
        "attention_item",
        "authority_grant",
        "command",
        "cost_entry",
        "dispatch_node",
        "engine_session",
        "evidence",
        "gate",
        "ingested_record",
        "lease",
        "message",
        "orchestrator_checkpoint",
        "pty_session",
        "pty_session_template",
        "receipt",
        "task",
        "workflow_run",
        "workspace_node",
        "worktree",
    ] {
        assert!(
            present.iter().any(|tag| tag == expected),
            "no {expected} row reached the canonical form: {present:?}"
        );
    }
    // Sorted by table then primary key, so the tags arrive grouped and in the
    // written-down order — the property the hash depends on.
    let mut sorted = present.clone();
    sorted.sort();
    assert_eq!(present, sorted, "the visit order is not stable");

    // A 64-bit counter is a decimal STRING all the way through. As a JSON
    // number `u64::MAX` comes back as 18446744073709552000.
    let text = String::from_utf8(records.clone()).expect("utf-8");
    assert!(
        text.contains("\"byte_size\":\"18446744073709551615\""),
        "evidence byte_size lost its decimal-string form"
    );
    assert!(
        text.contains("\"seq\":\"18446744073709551615\""),
        "orchestrator checkpoint seq lost its decimal-string form"
    );

    // Reading it again gives byte-identical bytes: nothing in the canonical
    // form reads a clock, a row order, or anything else that moves.
    assert_eq!(canonical(&store).await, records);

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn the_barrier_fires_on_either_bound_and_stores_what_it_hashed() {
    let maintenance = maintenance_pool().await;
    let (name, root, store) = checkpointing_store(&maintenance, "cpbarrier").await;
    populate(&store).await;

    // Neither bound has tripped: the interval is five minutes and the log is
    // nowhere near ten thousand events.
    assert!(checkpoints(&store).await.is_empty());

    // The TIME bound. Backdating the barrier is the honest way to test it —
    // the alternative is a test that sleeps for five minutes.
    sqlx::query("UPDATE gwk_internal.writer SET checkpoint_at = now() - interval '1 hour'")
        .execute(store.pool())
        .await
        .expect("backdate the barrier");
    apply(&store, "after-time", task("t-2")).await;

    let taken = checkpoints(&store).await;
    assert_eq!(taken.len(), 1, "the time bound must trip exactly once");
    let first = &taken[0];
    assert_eq!(first.schema_version, 1);
    assert_eq!(first.records_ref.media_type, RECORDS_MEDIA_TYPE);

    // The snapshot describes the state INCLUDING the append that triggered it:
    // it is taken inside that transaction, under the writer lock, so there is
    // no window where the log has an event the checkpoint has not seen.
    //
    // Hashed over the DERIVED records, not the canonical dump: `receipt` and
    // `attention_item` carry rows `submit` wrote beside the log — the paged
    // path writes both for a command that appends no event — so a digest over
    // them could never be reproduced by a replay.
    let records = derived(&store).await;
    assert_eq!(first.projection_hash, checkpoint::projection_hash(&records));

    // The records blob holds exactly the bytes that were hashed — which is why
    // its content address and the projection hash are one digest.
    // The SAME root the store is checkpointing into — `blob_store` would clear
    // it, which is right for a fresh case and wrong for reading back what this
    // one just wrote.
    let blobs = blob_store_with(&store, &root, common::TEST_KEK).await;
    let address = BlobAddress::parse(&first.records_ref.digest).expect("a legal address");
    assert_eq!(address.digest_hex(), first.projection_hash);
    let stored = read_all(&blobs, &address, first.records_ref.byte_size.value()).await;
    assert_eq!(stored, records);

    // The barrier moved with it, so the next append does not take another.
    apply(&store, "quiet", task("t-3")).await;
    assert_eq!(checkpoints(&store).await.len(), 1);

    // The EVENT bound. Jumping the sequence is the cheap equivalent of
    // appending ten thousand events.
    sqlx::query("UPDATE gwk_internal.writer SET next_seq = 20000")
        .execute(store.pool())
        .await
        .expect("jump the sequence");
    apply(&store, "after-count", task("t-4")).await;

    let taken = checkpoints(&store).await;
    assert_eq!(taken.len(), 2, "the event bound must trip");
    // Newest first, which is the order the recovery ladder walks.
    assert!(taken[0].through_sequence.value() > taken[1].through_sequence.value());

    // Sweep must not reclaim the records of a checkpoint that is still live —
    // that is a recovery that cannot run. The blob is referenced by no EVENT at
    // all, so only the checkpoint clause of the predicate saves it.
    let swept = blobs.sweep().await.expect("sweep");
    for checkpoint in &taken {
        let address = BlobAddress::parse(&checkpoint.records_ref.digest).expect("address");
        assert!(
            !swept.contains(&address),
            "swept a live checkpoint's records"
        );
        assert!(
            blobs.stat(&address).await.expect("stat").is_some(),
            "a live checkpoint's records were removed"
        );
    }

    drop_database(&maintenance, &name).await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_store_with_no_blob_home_takes_no_snapshots() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "cpnoblobs", 8).await;

    // No blob store attached, so there is nowhere to put the records. This is a
    // real configuration — `admin init` and the certifier drive the log with no
    // filesystem — and the barrier has to pass over it rather than fail every
    // append or write a checkpoint with no records behind it.
    sqlx::query("UPDATE gwk_internal.writer SET checkpoint_at = now() - interval '1 hour'")
        .execute(store.pool())
        .await
        .expect("backdate the barrier");
    apply(&store, "overdue", task("t-1")).await;

    assert!(store.blobs().is_none());
    assert!(checkpoints(&store).await.is_empty());

    drop_database(&maintenance, &name).await;
}

async fn checkpoints(store: &PgEventStore) -> Vec<gwk_domain::checkpoint::Checkpoint> {
    let mut conn = store.pool().acquire().await.expect("connection");
    checkpoint::checkpoints(&mut conn)
        .await
        .expect("read checkpoints")
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_clean_stop_leaves_a_checkpoint_the_next_start_can_verify() {
    let maintenance = maintenance_pool().await;
    let (name, root, store) = checkpointing_store(&maintenance, "cpshutdown").await;
    populate(&store).await;
    let watermark = store
        .watermark()
        .await
        .expect("watermark")
        .expect("a populated log");
    // Neither bound has tripped: a dozen commands is not ten thousand events and
    // the clock has not moved five minutes. So if a checkpoint lands at the
    // watermark below, the only thing that could have written it is the stop.
    assert!(
        checkpoints(&store).await.is_empty(),
        "the barrier fired on its own — this case would prove nothing"
    );

    let dir = runtime_dir("cpshutdown");
    let path = dir.join("gwk.sock");
    let listener = Listener::bind(&path).await.expect("bind");
    let daemon = Arc::new(Daemon::new(store, TEST_REVISION.to_owned()).expect("daemon"));
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let serving = tokio::spawn(async move {
        serve::run(listener, daemon, async move {
            let _ = stopped.await;
        })
        .await
    });

    let _ = stop.send(());
    let report = serving.await.expect("join").expect("a clean stop");
    assert_eq!(
        report.checkpoint,
        Some(watermark),
        "a clean stop must snapshot AT the watermark"
    );
    assert_eq!(report.checkpoint_error, None);

    // A fresh process, on the same database and the same blob root — which is
    // what a restart is. The checkpoint is only worth having if the next start
    // can read it and reach the strong verdict.
    let pool = connect_pool(&secret(&name), 4).await.expect("connect");
    let next = PgEventStore::open(pool).await.expect("open");
    let blobs = blob_store_with(&next, &root, TEST_KEK).await;
    let next = next.with_blobs(blobs);
    match next.recover().await.expect("recover").verdict {
        Verdict::Verified { anchor } => assert_eq!(anchor, watermark),
        other => panic!("a clean stop must restart into Verified, got {other:?}"),
    }

    drop_database(&maintenance, &name).await;
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&dir);
}
