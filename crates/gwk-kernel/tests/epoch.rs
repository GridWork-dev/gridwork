//! Certifies genesis, the seal, and the activation that breaks it.
//!
//! What only a real database can show: that genesis lands exactly once through
//! the fenced path and is idempotent under a second call, that a sealed kernel
//! refuses every business command while admitting activation alone, that the
//! same cutover replays without a second event while a different one is refused
//! by name, and that the aggregate-ownership rule — written for tasks — is what
//! keeps a foreign project from activating this kernel.
//!
//! `#[ignore]` because it needs a server — see `tests/common/mod.rs`.

mod common;

use common::{
    TEST_CUTOVER, TEST_MANIFEST_SHA256, TEST_REVISION, activation, actor, apply, drop_database,
    envelope, envelope_in, event_count, fresh_sealed_store, fresh_store, maintenance_pool,
    raw_store, total_event_count,
};
use gwk_domain::command::KernelCommand;
use gwk_domain::ids::TaskId;
use gwk_domain::protocol::{KernelErrorCode, KernelResult};
use gwk_kernel::store::PgEventStore;

fn task(id: &str) -> KernelCommand {
    KernelCommand::CreateTask {
        task_id: TaskId::new(id),
        kind: None,
        title: Some("t".to_owned()),
        spec_ref: None,
        project: None,
        priority: None,
        tracker_ref: None,
    }
}

fn activate(cutover_id: &str, manifest: &str) -> KernelCommand {
    KernelCommand::ActivateKernel {
        cutover_id: cutover_id.to_owned(),
        archive_manifest_sha256: manifest.to_owned(),
    }
}

/// Submit an already-built envelope and require a refusal.
async fn refuse_envelope(
    store: &PgEventStore,
    envelope: &gwk_domain::envelope::CommandEnvelope,
) -> (KernelErrorCode, String) {
    match store.submit(envelope).await {
        KernelResult::Error { code, message, .. } => (code, message),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn genesis_is_exactly_one_event_and_calling_again_appends_nothing() {
    let maintenance = maintenance_pool().await;
    let (name, store) = raw_store(&maintenance, "genesis", 64).await;

    assert_eq!(total_event_count(&store).await, 0);
    store.ensure_genesis(TEST_REVISION).await.expect("genesis");

    let row = sqlx::query_as::<_, (String, String, String, i64, Option<String>)>(
        "SELECT project_id, aggregate_id, event_type, aggregate_version, idempotency_key \
         FROM gwk.event WHERE aggregate_type = 'kernel'",
    )
    .fetch_one(store.pool())
    .await
    .expect("genesis event");
    assert_eq!(
        row,
        (
            "system".to_owned(),
            "singleton".to_owned(),
            "kernel_initialized".to_owned(),
            1,
            Some("kernel_initialized:v1".to_owned()),
        )
    );

    // Contract version and revision, and nothing else — the payload is
    // immutable, so anything extra is permanent.
    let payload: serde_json::Value =
        sqlx::query_scalar("SELECT payload FROM gwk.event WHERE aggregate_type = 'kernel'")
            .fetch_one(store.pool())
            .await
            .expect("genesis payload");
    assert_eq!(
        payload,
        serde_json::json!({ "contract_version": 1, "public_revision": TEST_REVISION })
    );

    // The sequence is DATABASE-assigned and never asserted to be 1 (ADR 0026);
    // what is asserted is that genesis is the whole log.
    assert_eq!(total_event_count(&store).await, 1);

    store
        .ensure_genesis(TEST_REVISION)
        .await
        .expect("second genesis is a no-op");
    assert_eq!(total_event_count(&store).await, 1);

    drop(store);
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn genesis_refuses_a_revision_it_could_not_later_resolve() {
    let maintenance = maintenance_pool().await;
    let (name, store) = raw_store(&maintenance, "genesisrev", 64).await;

    for bad in ["0123456", "", &"A".repeat(40)] {
        let refusal = store
            .ensure_genesis(bad)
            .await
            .expect_err("an unusable revision");
        assert_eq!(refusal.code, KernelErrorCode::Validation);
    }
    assert_eq!(total_event_count(&store).await, 0);

    drop(store);
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_kernel_without_an_epoch_admits_nothing_at_all() {
    let maintenance = maintenance_pool().await;
    let (name, store) = raw_store(&maintenance, "noepoch", 64).await;

    let (code, message) = refuse_envelope(&store, &envelope("t", &task("t-1"))).await;
    assert_eq!(code, KernelErrorCode::Sealed);
    assert!(message.contains("no epoch"), "{message}");

    // Including activation: it asserts version 1 and there is no version 1, so
    // an epoch-less kernel is not a kernel one cutover away from working.
    let (code, message) = refuse_envelope(&store, &activation(TEST_CUTOVER)).await;
    assert_eq!(code, KernelErrorCode::Sealed);
    assert!(message.contains("no epoch"), "{message}");
    assert_eq!(total_event_count(&store).await, 0);

    drop(store);
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_sealed_kernel_refuses_business_commands_and_admits_activation_alone() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_sealed_store(&maintenance, "sealed", 64).await;

    let (code, message) = refuse_envelope(&store, &envelope("t", &task("t-1"))).await;
    assert_eq!(code, KernelErrorCode::Sealed);
    assert!(message.contains("create_task"), "{message}");
    // Refused before the write, not after: the seal is a gate, not a filter on
    // the way out.
    assert_eq!(event_count(&store).await, 0);

    let events = match store.submit(&activation(TEST_CUTOVER)).await {
        KernelResult::CommandApplied { events, .. } => events,
        other => panic!("expected CommandApplied, got {other:?}"),
    };
    assert_eq!(events[0].event_type, "kernel_activated");
    assert_eq!(events[0].aggregate_version, 2);

    // And now the command that was refused a moment ago is ordinary work.
    apply(&store, "t", task("t-1")).await;
    assert_eq!(event_count(&store).await, 1);

    drop(store);
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn the_same_cutover_retries_stably_and_a_different_one_is_refused_by_name() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "cutover", 64).await;

    // A cutover is retried after a network failure the operator cannot
    // distinguish from a refusal. Byte-identical answer, no second event.
    let first: i64 = total_event_count(&store).await;
    match store.submit(&activation(TEST_CUTOVER)).await {
        KernelResult::CommandApplied { events, .. } => {
            assert_eq!(events[0].aggregate_version, 2);
        }
        other => panic!("expected the original events, got {other:?}"),
    }
    assert_eq!(total_event_count(&store).await, first);

    // A DIFFERENT cutover is the case that must never be answered by a bare
    // version number: the operator is told which epoch they are standing in.
    let (code, message) = refuse_envelope(&store, &activation("cutover-second")).await;
    assert_eq!(code, KernelErrorCode::AlreadyActive);
    assert!(message.contains(TEST_CUTOVER), "{message}");
    assert!(message.contains("cutover-second"), "{message}");
    assert_eq!(total_event_count(&store).await, first);

    // The same cutover with a different manifest is neither: the key is taken
    // by a request whose body differs, which is what the key is for.
    let mut tampered = activation(TEST_CUTOVER);
    let command = activate(TEST_CUTOVER, &"b".repeat(64));
    tampered.payload = serde_json::to_value(&command).expect("payload");
    let (code, _) = refuse_envelope(&store, &tampered).await;
    assert_eq!(code, KernelErrorCode::IdempotencyConflict);
    assert_eq!(total_event_count(&store).await, first);

    drop(store);
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn an_activation_must_carry_the_key_and_the_digest_its_cutover_implies() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_sealed_store(&maintenance, "actshape", 64).await;

    // The key names the cutover, so a caller cannot make two cutovers share
    // one retry identity.
    let mut wrong_key = activation(TEST_CUTOVER);
    wrong_key.idempotency_key = gwk_domain::ids::IdempotencyKey::new("whatever");
    let (code, message) = refuse_envelope(&store, &wrong_key).await;
    assert_eq!(code, KernelErrorCode::Validation);
    assert!(
        message.contains("kernel_activated:cutover-test"),
        "{message}"
    );

    // The manifest digest is checked here because the event is immutable: a
    // malformed digest committed at the cutover boundary cannot be corrected.
    let mut bad_digest = activation(TEST_CUTOVER);
    let command = activate(TEST_CUTOVER, "not-a-digest");
    bad_digest.payload = serde_json::to_value(&command).expect("payload");
    let (code, message) = refuse_envelope(&store, &bad_digest).await;
    assert_eq!(code, KernelErrorCode::Validation);
    assert!(message.contains("64-hex"), "{message}");

    assert_eq!(total_event_count(&store).await, 1);

    drop(store);
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn only_the_project_that_wrote_genesis_may_activate() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_sealed_store(&maintenance, "actowner", 64).await;

    // Nothing about activation says "system" anywhere in this path — the
    // ownership rule written for tasks answers it, because genesis is the
    // kernel aggregate's first event and it was written under `system`.
    let foreign = envelope_in(
        "p",
        &format!("kernel_activated:{TEST_CUTOVER}"),
        actor("kernel"),
        &activate(TEST_CUTOVER, TEST_MANIFEST_SHA256),
    );
    let (code, message) = refuse_envelope(&store, &foreign).await;
    assert_eq!(code, KernelErrorCode::Validation);
    assert!(
        message.contains("belongs to project \"system\""),
        "{message}"
    );
    assert_eq!(total_event_count(&store).await, 1);

    // And the real one still works afterwards.
    match store.submit(&activation(TEST_CUTOVER)).await {
        KernelResult::CommandApplied { .. } => {}
        other => panic!("expected CommandApplied, got {other:?}"),
    }

    drop(store);
    drop_database(&maintenance, &name).await;
}
