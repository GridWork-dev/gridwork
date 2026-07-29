//! Certifies recovery: what a restart proves, what it refuses to claim, and
//! what it must never write.
//!
//! What only a real database can show: that a checkpoint sitting exactly at the
//! watermark verifies the live projections, that a log which outran its
//! checkpoint says so instead of implying a check it did not run, that an
//! unreadable checkpoint costs the ladder one rung rather than every rung, that
//! a log restored WITHOUT its projections is rebuilt byte-identically by
//! replaying it, that a forged projection row blocks readiness, and that no
//! path through any of it appends an event.
//!
//! `#[ignore]` because it needs a server — see `tests/common/mod.rs`.

mod common;

use common::{
    apply, blob_store_with, checkpointing_store, drop_database, maintenance_pool, populate,
    raw_store, task, total_event_count,
};
use gwk_domain::blob::BlobAddress;
use gwk_domain::command::KernelCommand;
use gwk_domain::fsm::AttemptState;
use gwk_domain::ids::{AttemptId, EngineId, TaskId};
use gwk_domain::port::{BlobStore, EventStore};
use gwk_kernel::checkpoint;
use gwk_kernel::recover::Verdict;
use gwk_kernel::store::PgEventStore;
use sqlx::{PgPool, Row};

/// Force the barrier by backdating it, then append one command so it trips.
///
/// Backdating is the honest way to reach the time bound — the alternative is a
/// test that sleeps for five minutes — and the checkpoint it produces sits at
/// the watermark, which is what a clean shutdown would also leave behind.
async fn checkpoint_now(store: &PgEventStore, key: &str, id: &str) {
    sqlx::query("UPDATE gwk_internal.writer SET checkpoint_at = now() - interval '1 hour'")
        .execute(store.pool())
        .await
        .expect("backdate the barrier");
    apply(store, key, task(id)).await;
}

async fn live_hash(store: &PgEventStore) -> String {
    let mut conn = store.pool().acquire().await.expect("connection");
    checkpoint::projection_hash(
        &checkpoint::derived_records(&mut conn)
            .await
            .expect("derived records"),
    )
}

async fn checkpoints(store: &PgEventStore) -> Vec<gwk_domain::checkpoint::Checkpoint> {
    let mut conn = store.pool().acquire().await.expect("connection");
    checkpoint::checkpoints(&mut conn)
        .await
        .expect("read checkpoints")
}

/// A database holding `source`'s log and NOTHING else.
///
/// This is the restore the module is written for: a backup that brought the
/// events back without the projections. Copying whole rows through `to_jsonb`
/// rather than a hand-written column list means a column added to `gwk.event`
/// cannot silently stop being copied — `seq` is cast to text on the way out
/// because a `numeric` renders as a JSON number, which is the same precision
/// trap the canonical records dodge.
async fn log_only_clone(
    maintenance: &PgPool,
    source: &PgEventStore,
    tag: &str,
) -> (String, PgEventStore) {
    let (name, clone) = raw_store(maintenance, tag, 8).await;
    let rows = sqlx::query(
        "SELECT (to_jsonb(e) || jsonb_build_object('seq', e.seq::text))::text \
         FROM gwk.event e ORDER BY seq",
    )
    .fetch_all(source.pool())
    .await
    .expect("read the source log");
    for row in &rows {
        let event: String = row.get(0);
        sqlx::query(
            "INSERT INTO gwk.event \
             SELECT * FROM jsonb_populate_record(null::gwk.event, $1::jsonb)",
        )
        .bind(&event)
        .execute(clone.pool())
        .await
        .expect("copy an event");
    }
    (name, clone)
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_checkpoint_at_the_watermark_is_what_lets_a_restart_prove_anything() {
    let maintenance = maintenance_pool().await;
    let (name, root, store) = checkpointing_store(&maintenance, "rcverified").await;
    populate(&store).await;
    checkpoint_now(&store, "trip", "t-2").await;

    let report = store.recover().await.expect("recover");
    let taken = checkpoints(&store).await;
    assert_eq!(taken.len(), 1);
    assert_eq!(
        report.verdict,
        Verdict::Verified {
            anchor: taken[0].through_sequence
        },
        "a checkpoint at the watermark is the one case that can be checked in place"
    );
    assert_eq!(report.watermark, Some(taken[0].through_sequence));
    assert_eq!(report.live_hash, taken[0].projection_hash);
    assert!(report.ready());
    assert!(report.rejected.is_empty());

    drop_database(&maintenance, &name).await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_log_that_outran_its_checkpoint_says_so_rather_than_implying_a_check() {
    let maintenance = maintenance_pool().await;
    let (name, root, store) = checkpointing_store(&maintenance, "rcbehind").await;
    populate(&store).await;
    checkpoint_now(&store, "trip", "t-2").await;
    let anchor = checkpoints(&store).await[0].through_sequence;

    // One more append and the checkpoint is behind. There is no way to derive
    // what the projections SHOULD hash to now without replaying, and replaying
    // in place is what the schema forbids.
    apply(&store, "after", task("t-3")).await;
    let watermark = store.watermark().await.expect("watermark").expect("a log");
    assert!(watermark.value() > anchor.value());

    let report = store.recover().await.expect("recover");
    match &report.verdict {
        Verdict::Unverified { reason } => {
            assert!(
                reason.contains(&anchor.value().to_string())
                    && reason.contains(&watermark.value().to_string()),
                "the reason must name both ends of the gap it could not close: {reason}"
            );
        }
        other => panic!("expected Unverified, got {other:?}"),
    }
    // Unverified is a statement about this run, not an accusation: these
    // projections were written in the same transactions as their events and are
    // correct. Refusing to serve here would turn every crash into an outage.
    assert!(report.ready());

    drop_database(&maintenance, &name).await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn an_unreadable_checkpoint_is_walked_past_and_reported() {
    let maintenance = maintenance_pool().await;
    let (name, root, store) = checkpointing_store(&maintenance, "rcladder").await;
    populate(&store).await;
    checkpoint_now(&store, "trip-1", "t-2").await;
    checkpoint_now(&store, "trip-2", "t-3").await;

    let taken = checkpoints(&store).await;
    assert_eq!(taken.len(), 2, "newest first");
    let newest = &taken[0];

    // Shred the newest checkpoint's records. Its key is destroyed, so the bytes
    // are unrecoverable — the same state a lost or corrupted records blob
    // leaves, reached deterministically.
    let blobs = blob_store_with(&store, &root, common::TEST_KEK).await;
    let address = BlobAddress::parse(&newest.records_ref.digest).expect("address");
    blobs.shred(&address).await.expect("shred");

    let report = store.recover().await.expect("recover");
    // One bad checkpoint costs one rung, not the whole ladder.
    assert_eq!(
        report.rejected.len(),
        1,
        "exactly the newest should have been rejected: {:?}",
        report.rejected
    );
    assert_eq!(report.rejected[0].0, newest.through_sequence);
    assert!(
        !report.rejected[0].1.is_empty(),
        "a rejection has to say why"
    );
    // It fell back to the older one, which is behind the watermark — so the
    // verdict is honest about not having checked, and the kernel still serves.
    assert!(matches!(report.verdict, Verdict::Unverified { .. }));
    assert!(report.ready());

    drop_database(&maintenance, &name).await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_log_restored_without_its_projections_is_rebuilt_by_replay() {
    let maintenance = maintenance_pool().await;
    let (name, root, store) = checkpointing_store(&maintenance, "rccold").await;
    populate(&store).await;
    checkpoint_now(&store, "trip", "t-2").await;
    let expected = live_hash(&store).await;

    let (clone_name, clone) = log_only_clone(&maintenance, &store, "rccoldclone").await;
    assert_eq!(live_hash(&clone).await, checkpoint::projection_hash(&[]));

    let report = clone.recover().await.expect("recover");
    match report.verdict {
        Verdict::Replayed { events } => assert!(events > 0, "a populated log replays events"),
        other => panic!("expected Replayed, got {other:?}"),
    }
    assert!(report.ready());

    // The whole argument for `apply_event` being the only projection writer:
    // replaying the log reproduces the projections BYTE for byte, so the hash a
    // checkpoint recorded is still the right answer on a different database.
    assert_eq!(
        live_hash(&clone).await,
        expected,
        "replay did not reproduce the projections it is supposed to be able to rebuild"
    );

    drop_database(&maintenance, &clone_name).await;
    drop_database(&maintenance, &name).await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn rebuild_into_compares_against_live_and_changes_nothing() {
    let maintenance = maintenance_pool().await;
    let (name, store) = common::fresh_store(&maintenance, "rcrebuild", 8).await;
    populate(&store).await;
    let before = live_hash(&store).await;
    let watermark = store.watermark().await.expect("watermark");

    let (scratch_name, scratch) = raw_store(&maintenance, "rcscratch", 8).await;
    let report = store
        .rebuild_into(scratch.pool())
        .await
        .expect("rebuild into scratch");
    assert!(report.agrees, "a healthy log must rebuild to what it built");
    assert_eq!(report.live_hash, before);
    assert_eq!(report.rebuilt_hash, before);
    assert_eq!(report.through_sequence, watermark);

    // It never swaps anything, and it never touches live: replacing the
    // projections is an operator act with its own blast radius.
    assert_eq!(live_hash(&store).await, before);
    // And the rebuilt rows are really there, in the scratch, for the operator
    // to look at.
    assert_eq!(live_hash(&scratch).await, before);

    // A second rebuild into the same scratch is refused rather than layered on
    // top: a comparison against a mixture proves nothing.
    let refusal = store
        .rebuild_into(scratch.pool())
        .await
        .expect_err("a nonempty scratch must be refused");
    assert!(
        refusal.message.contains("empty"),
        "the refusal must say what is wrong: {}",
        refusal.message
    );

    drop_database(&maintenance, &scratch_name).await;
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_forged_projection_row_blocks_readiness() {
    let maintenance = maintenance_pool().await;
    let (name, root, store) = checkpointing_store(&maintenance, "rcforged").await;
    populate(&store).await;
    checkpoint_now(&store, "trip", "t-2").await;
    let recorded = checkpoints(&store).await[0].projection_hash.clone();
    assert!(store.recover().await.expect("recover").ready());

    // Reach around the log and edit a projection directly. Nothing in the
    // kernel can do this — it is the hand-edited row, or the bad restore, that
    // recovery exists to catch.
    sqlx::query("UPDATE gwk.evidence SET kind = 'forged' WHERE id = 'ev-1'")
        .execute(store.pool())
        .await
        .expect("forge a row");

    let report = store.recover().await.expect("recover");
    match &report.verdict {
        Verdict::Diverged { expected, found } => {
            assert_eq!(expected, &recorded);
            assert_ne!(found, &recorded);
            assert_eq!(found, &report.live_hash);
        }
        other => panic!("expected Diverged, got {other:?}"),
    }
    assert!(
        !report.ready(),
        "serving is the one thing that must not happen next"
    );

    drop_database(&maintenance, &name).await;
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn uncertainty_comes_from_the_fsm_and_not_from_a_guess() {
    let maintenance = maintenance_pool().await;
    let (name, store) = common::fresh_store(&maintenance, "rcuncertain", 8).await;
    populate(&store).await;

    // a-1 arrives from `populate` and stays queued.
    for (id, key) in [("a-2", "mk2"), ("a-3", "mk3"), ("a-4", "mk4")] {
        apply(
            &store,
            key,
            KernelCommand::CreateAttempt {
                attempt_id: AttemptId::new(id),
                task_id: TaskId::new("t-1"),
                engine: EngineId::new("engine-a"),
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
    }
    // a-2 all the way to running, a-3 only as far as leased, a-4 to starting.
    // The version each transition expects is the one the previous one left.
    for (id, key, to, version) in [
        ("a-2", "a2l", AttemptState::Leased, 1),
        ("a-2", "a2s", AttemptState::Starting, 2),
        ("a-2", "a2r", AttemptState::Running, 3),
        ("a-3", "a3l", AttemptState::Leased, 1),
        ("a-4", "a4l", AttemptState::Leased, 1),
        ("a-4", "a4s", AttemptState::Starting, 2),
    ] {
        apply(
            &store,
            key,
            KernelCommand::TransitionAttempt {
                attempt_id: AttemptId::new(id),
                to,
                expected_version: version,
                receipt_id: None,
            },
        )
        .await;
    }

    let report = store.recover().await.expect("recover");
    // `starting` and `running` have a legal edge to `unknown`; `queued` and
    // `leased` do not. An attempt that never started has no outcome to be
    // uncertain about — it simply has not run — and reporting it would push an
    // operator toward burying work that is fine.
    assert_eq!(report.uncertain, vec!["a-2".to_owned(), "a-4".to_owned()]);

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn no_recovery_path_writes_an_outcome_event() {
    let maintenance = maintenance_pool().await;
    let (name, root, store) = checkpointing_store(&maintenance, "rcnowrite").await;
    populate(&store).await;
    apply(
        &store,
        "inflight",
        KernelCommand::TransitionAttempt {
            attempt_id: AttemptId::new("a-1"),
            to: AttemptState::Leased,
            expected_version: 1,
            receipt_id: None,
        },
    )
    .await;

    // Every verdict, one store at a time, each asserted against the log it
    // started with. `unknown` is a CLAIM about an outcome the kernel never
    // observed; recovery may report an uncertain attempt but must never be the
    // thing that decides one.
    let unverified = store.recover().await.expect("recover");
    assert!(matches!(unverified.verdict, Verdict::Unverified { .. }));
    let after_unverified = total_event_count(&store).await;

    checkpoint_now(&store, "trip", "t-2").await;
    let before_verified = total_event_count(&store).await;
    let verified = store.recover().await.expect("recover");
    assert!(matches!(verified.verdict, Verdict::Verified { .. }));
    assert_eq!(total_event_count(&store).await, before_verified);
    assert!(
        after_unverified < before_verified,
        "only the append moved it"
    );

    // The cold path is the only one that writes at all, and what it writes is
    // projections through `apply_event` — never an event.
    let (clone_name, clone) = log_only_clone(&maintenance, &store, "rcnowriteclone").await;
    let before_replay = total_event_count(&clone).await;
    let replayed = clone.recover().await.expect("recover");
    assert!(matches!(replayed.verdict, Verdict::Replayed { .. }));
    assert_eq!(
        total_event_count(&clone).await,
        before_replay,
        "a replay re-derives projections from the log; it does not extend it"
    );

    // The uncertain attempt is still exactly where the log left it. Nothing
    // transitioned it to `unknown` on its behalf.
    let state: String = sqlx::query_scalar("SELECT state FROM gwk.attempt WHERE id = 'a-1'")
        .fetch_one(clone.pool())
        .await
        .expect("attempt state");
    assert_eq!(state, "leased");

    drop_database(&maintenance, &clone_name).await;
    drop_database(&maintenance, &name).await;
    let _ = std::fs::remove_dir_all(&root);
}
