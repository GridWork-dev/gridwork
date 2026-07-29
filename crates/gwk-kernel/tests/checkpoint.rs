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

use common::{
    PROJECT, actor, apply, blob_store_with, checkpointing_store, drop_database, fresh_store,
    maintenance_pool, read_all,
};
use gwk_domain::blob::BlobAddress;
use gwk_domain::command::KernelCommand;
use gwk_domain::fsm::LeaseMode;
use gwk_domain::ids::{
    AttemptId, AttentionItemId, AuthorityGrantId, ByteCount, CommandId, DispatchNodeId, EngineId,
    EngineSessionId, EvidenceId, GateId, LeaseId, MessageId, Seq, TaskId, WorktreeId,
};
use gwk_domain::inherited::OrchestratorCheckpoint;
use gwk_domain::port::BlobStore;
use gwk_kernel::checkpoint::{self, RECORDS_MEDIA_TYPE};
use gwk_kernel::store::PgEventStore;

/// One row in every projection table the kernel can reach today.
///
/// Deliberately exhaustive: `canonical_records` only exercises a table that has
/// rows in it, so an empty table proves nothing about whether its columns still
/// match its contract type. Any projection missing from here is a parity check
/// nobody is running.
async fn populate(store: &PgEventStore) {
    apply(
        store,
        "task",
        KernelCommand::CreateTask {
            task_id: TaskId::new("t-1"),
            kind: Some("phase".into()),
            title: Some("ship the kernel".into()),
            spec_ref: None,
            project: Some(PROJECT.to_owned()),
            priority: Some(3),
            tracker_ref: None,
        },
    )
    .await;
    apply(
        store,
        "attempt",
        KernelCommand::CreateAttempt {
            attempt_id: AttemptId::new("a-1"),
            task_id: TaskId::new("t-1"),
            engine: EngineId::new("engine-a"),
            capability: Some("code_write".into()),
            role: None,
            model_lane: Some("standard".into()),
            permission_profile: None,
            worktree_lease_id: None,
            base_sha: None,
            budget: None,
        },
    )
    .await;
    apply(
        store,
        "session",
        KernelCommand::OpenEngineSession {
            engine_session_id: EngineSessionId::new("s-1"),
            attempt_id: AttemptId::new("a-1"),
            engine: EngineId::new("engine-a"),
            provider_session_ref: Some("prov-1".into()),
        },
    )
    .await;
    apply(
        store,
        "node",
        KernelCommand::RegisterDispatchNode {
            dispatch_node_id: DispatchNodeId::new("n-1"),
            parent_id: None,
            attempt_id: Some(AttemptId::new("a-1")),
            kind: "subagent".into(),
            label: Some("reviewer".into()),
        },
    )
    .await;
    apply(
        store,
        "lease",
        KernelCommand::AcquireLease {
            lease_id: LeaseId::new("l-1"),
            mode: LeaseMode::Exclusive,
            holder: Some("a-1".into()),
            scope: Some("worktree".into()),
            repo: Some("gridwork".into()),
            path: Some("/w/kernel".into()),
            branch: Some("feature/kernel".into()),
            base_sha: None,
            expires_at: Some(gwk_domain::ids::Timestamp::new("2026-07-28T01:00:00Z")),
        },
    )
    .await;
    apply(
        store,
        "worktree",
        KernelCommand::RegisterWorktree {
            worktree_id: WorktreeId::new("wt-1"),
            repo: "gridwork".into(),
            path: "/w/kernel".into(),
            branch: "feature/kernel".into(),
            base_sha: None,
            lease_id: Some(LeaseId::new("l-1")),
        },
    )
    .await;
    apply(
        store,
        "message",
        KernelCommand::SendMessage {
            message_id: MessageId::new("m-1"),
            correlation_id: None,
            reply_to: None,
            sender: Some("orchestrator".into()),
            recipient: Some("engine-a".into()),
            channel: Some("dispatch".into()),
            kind: Some("brief".into()),
            payload: Some(serde_json::json!({ "goal": "ship the kernel" })),
            deadline: None,
        },
    )
    .await;
    apply(
        store,
        "gate",
        KernelCommand::OpenGate {
            gate_id: GateId::new("g-1"),
            attempt_id: None,
            phase_ref: Some("4p-kernel".into()),
            kind: Some("review".into()),
        },
    )
    .await;
    // `issue_command` is in the authority risk table, so the grant comes first
    // — and it is the row that populates `authority_grant`.
    apply(
        store,
        "grant",
        KernelCommand::GrantAuthority {
            authority_grant_id: AuthorityGrantId::new("g-stop"),
            grantee: actor("kernel"),
            action_class: "stop".to_owned(),
            scope: None,
            expires_at: None,
        },
    )
    .await;
    // Applying it also writes the receipt every authority decision leaves.
    apply(
        store,
        "command",
        KernelCommand::IssueCommand {
            command_id: CommandId::new("c-1"),
            kind: "stop_attempt".to_owned(),
            targets: vec!["a-1".to_owned()],
            actor: None,
        },
    )
    .await;
    apply(
        store,
        "evidence",
        KernelCommand::RecordEvidence {
            evidence_id: EvidenceId::new("ev-1"),
            kind: "diff".to_owned(),
            r#ref: "blob://sha256-abc".to_owned(),
            digest: Some("sha256-abc".into()),
            // The whole reason the column is `numeric(20,0)` and the canonical
            // form casts it to text: as a JSON number this loses precision
            // above 2^53 and comes back a DIFFERENT value, silently.
            byte_size: Some(ByteCount::new(u64::MAX)),
        },
    )
    .await;
    apply(
        store,
        "attention",
        KernelCommand::RaiseAttention {
            attention_item_id: AttentionItemId::new("att-1"),
            kind: "risk_tag".to_owned(),
            summary: "data-migration pages".to_owned(),
            subject_ref: Some("task/t-1".to_owned()),
            raised_by: Some(actor("kernel")),
        },
    )
    .await;
    apply(
        store,
        "orch",
        KernelCommand::WriteOrchestratorCheckpoint {
            checkpoint: OrchestratorCheckpoint {
                orchestrator_id: Some("orch-1".into()),
                seq: Seq::new(u64::MAX),
                native_session_ref: None,
                active_goal: Some("ship".into()),
                active_step_ref: None,
                latest_command_ref: None,
                open_attempts: Some(vec![]),
                leases: None,
                pending_approvals: None,
                budget_cursor: None,
            },
        },
    )
    .await;
}

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
        "dispatch_node",
        "engine_session",
        "evidence",
        "gate",
        "lease",
        "message",
        "orchestrator_checkpoint",
        "receipt",
        "task",
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
    let records = canonical(&store).await;
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

async fn checkpoints(store: &PgEventStore) -> Vec<gwk_domain::checkpoint::Checkpoint> {
    let mut conn = store.pool().acquire().await.expect("connection");
    checkpoint::checkpoints(&mut conn)
        .await
        .expect("read checkpoints")
}
