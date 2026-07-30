//! Certifies the work-lifecycle command path against a real server.
//!
//! What only a real database can show: that the event and its projection land
//! in ONE transaction, that the row's version and the log's stay equal, that
//! the DDL's own guards fire when the Rust path is bypassed, and that a keyed
//! retry answers from the log instead of applying twice.
//!
//! `#[ignore]` because it needs a server — see `tests/common/mod.rs`.

mod common;

use common::{
    PROJECT, actor, apply, drop_database, envelope, envelope_as, envelope_in, event_count,
    fresh_store, maintenance_pool, refuse, state_row,
};
use gwk_domain::command::KernelCommand;
use gwk_domain::entity::Budget;
use gwk_domain::envelope::INLINE_PAYLOAD_MAX_BYTES;
use gwk_domain::fsm::{AttemptState, LeaseMode, TaskState};
use gwk_domain::ids::{
    AggregateId, AttemptId, DispatchNodeId, EngineId, EngineSessionId, LeaseId, ReceiptId, Seq,
    TaskId, Timestamp, WorktreeId,
};
use gwk_domain::inherited::{FindingAction, OrchestratorCheckpoint, RoundFindingSummary};
use gwk_domain::protocol::{KernelErrorCode, KernelResult};
use gwk_domain::transition::LIVENESS_PRODUCER_KIND;
use gwk_kernel::store::PgEventStore;
use sqlx::Row;

/// The projection's own view of a row's state and version.
async fn task_row(store: &PgEventStore, id: &str) -> (String, i64) {
    state_row(
        store,
        "SELECT state, version FROM gwk.task WHERE id = $1",
        id,
    )
    .await
}

async fn attempt_row(store: &PgEventStore, id: &str) -> (String, i64) {
    state_row(
        store,
        "SELECT state, version FROM gwk.attempt WHERE id = $1",
        id,
    )
    .await
}

fn task(id: &str) -> KernelCommand {
    task_in(PROJECT, id)
}

/// The body's own `project` has to track the envelope's, so a case that
/// submits in another project builds its command there too.
fn task_in(project: &str, id: &str) -> KernelCommand {
    KernelCommand::CreateTask {
        task_id: TaskId::new(id),
        kind: Some("execution".into()),
        title: Some("ship the kernel".into()),
        spec_ref: None,
        project: Some(project.into()),
        priority: Some(2),
        tracker_ref: None,
    }
}

fn attempt(id: &str, task_id: &str) -> KernelCommand {
    KernelCommand::CreateAttempt {
        attempt_id: AttemptId::new(id),
        task_id: TaskId::new(task_id),
        engine: EngineId::new("engine-a"),
        capability: Some("code_write".into()),
        role: None,
        model_lane: Some("standard".into()),
        permission_profile: None,
        worktree_lease_id: None,
        base_sha: None,
        budget: None,
    }
}

/// Drive an attempt from `queued` to `running` — the prefix every flip case
/// needs, and the shortest path through the accepted edge table.
async fn run_attempt(store: &PgEventStore, id: &str, tag: &str) {
    for (step, (to, version)) in [
        (AttemptState::Leased, 1),
        (AttemptState::Starting, 2),
        (AttemptState::Running, 3),
    ]
    .into_iter()
    .enumerate()
    {
        apply(
            store,
            &format!("{tag}-step-{step}"),
            KernelCommand::TransitionAttempt {
                attempt_id: AttemptId::new(id),
                to,
                expected_version: version,
                receipt_id: None,
            },
        )
        .await;
    }
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn the_work_lifecycle_lands_as_events_and_projections_together() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "lifecycle", 64).await;

    // A task, an attempt under it, and the attempt walked to running.
    apply(&store, "task", task("t-1")).await;
    assert_eq!(task_row(&store, "t-1").await, ("submitted".to_owned(), 1));
    apply(&store, "attempt", attempt("a-1", "t-1")).await;
    run_attempt(&store, "a-1", "run").await;
    assert_eq!(attempt_row(&store, "a-1").await, ("running".to_owned(), 4));

    // A lease, and a worktree that references it — the projection carries the
    // route snapshot, so the FK has to resolve against a row that really exists.
    apply(
        &store,
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
            expires_at: Some(Timestamp::new("2026-07-28T01:00:00Z")),
        },
    )
    .await;
    apply(
        &store,
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
        &store,
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
        &store,
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
        &store,
        "budget",
        KernelCommand::UpdateBudget {
            attempt_id: AttemptId::new("a-1"),
            expected_version: 4,
            budget: Budget {
                max_tokens: Some(2_000_000),
                max_tool_calls: None,
                max_wall_ms: None,
                max_cost_micros: None,
            },
        },
    )
    .await;
    assert_eq!(attempt_row(&store, "a-1").await, ("running".to_owned(), 5));

    // Release the worktree: the projection has to be able to say so, which is
    // the whole reason the columns exist.
    apply(
        &store,
        "release-wt",
        KernelCommand::ReleaseWorktree {
            worktree_id: WorktreeId::new("wt-1"),
            disposition: Some("clean".into()),
        },
    )
    .await;
    let released: (Option<String>, Option<String>) = {
        let row = sqlx::query(
            "SELECT to_json(released_at) #>> '{}' AS at, disposition FROM gwk.worktree WHERE id = $1",
        )
        .bind("wt-1")
        .fetch_one(store.pool())
        .await
        .expect("worktree row");
        (row.get("at"), row.get("disposition"))
    };
    assert!(released.0.is_some(), "a released worktree carries a stamp");
    assert_eq!(released.1.as_deref(), Some("clean"));

    // The checkpoint is latest-per-orchestrator, so a second write replaces the
    // first rather than accumulating.
    for (key, seq, goal) in [("cp-1", 5u64, "spec"), ("cp-2", 9, "execute")] {
        apply(
            &store,
            key,
            KernelCommand::WriteOrchestratorCheckpoint {
                checkpoint: OrchestratorCheckpoint {
                    orchestrator_id: Some("orch-1".into()),
                    seq: Seq::new(seq),
                    native_session_ref: None,
                    active_goal: Some(goal.into()),
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
    let checkpoint: (String, String, i64) = {
        let row = sqlx::query(
            "SELECT seq::text AS seq, active_goal, \
             (SELECT count(*) FROM gwk.orchestrator_checkpoint) AS rows \
             FROM gwk.orchestrator_checkpoint WHERE orchestrator_id = $1",
        )
        .bind("orch-1")
        .fetch_one(store.pool())
        .await
        .expect("checkpoint row");
        (row.get("seq"), row.get("active_goal"), row.get("rows"))
    };
    assert_eq!(checkpoint, ("9".to_owned(), "execute".to_owned(), 1));

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_failed_projection_takes_its_event_down_with_it() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "atomic", 64).await;

    // The attempt's event is well-formed and the append itself succeeds — it is
    // the PROJECTION that cannot land, because `task_id` references a task that
    // was never created. If the two were not one transaction, the log would now
    // hold an event no projection reflects, and every later replay would
    // reproduce the same broken write.
    let (code, message) = refuse(&store, "orphan", attempt("a-1", "missing")).await;
    assert_eq!(code, KernelErrorCode::NotFound, "{message}");
    assert_eq!(
        event_count(&store).await,
        0,
        "the event survived a projection that did not"
    );

    // And the store still works afterwards: the rollback released the writer
    // lock and did not consume a sequence.
    apply(&store, "task", task("t-1")).await;
    assert_eq!(event_count(&store).await, 1);

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_retried_command_answers_from_the_log_and_a_reused_key_is_refused() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "idempotent", 64).await;

    let first = apply(&store, "same-key", task("t-1")).await;
    // The identical request again. The projection must NOT be touched: the row
    // is at version 1 and re-applying would drive it to 2 with no event behind
    // that number.
    let replay = apply(&store, "same-key", task("t-1")).await;
    assert_eq!(
        first, replay,
        "a retry must answer with the ORIGINAL events"
    );
    assert_eq!(event_count(&store).await, 1);
    assert_eq!(task_row(&store, "t-1").await, ("submitted".to_owned(), 1));

    // The same key aimed at a DIFFERENT aggregate is a reuse, not a retry. The
    // per-aggregate index cannot see this case — the project-wide one can.
    let (code, message) = refuse(&store, "same-key", task("t-2")).await;
    assert_eq!(code, KernelErrorCode::IdempotencyConflict, "{message}");
    assert_eq!(event_count(&store).await, 1, "the reuse landed anyway");

    // Same aggregate, different body, same key: also a reuse.
    let (code, _) = refuse(
        &store,
        "same-key",
        KernelCommand::TransitionTask {
            task_id: TaskId::new("t-1"),
            to: TaskState::Working,
            expected_version: 1,
        },
    )
    .await;
    assert_eq!(code, KernelErrorCode::IdempotencyConflict);

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_key_taken_on_this_aggregate_by_another_project_is_a_conflict_not_a_replay() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "crossproject", 64).await;

    // Idempotency is scoped per project, but the aggregate namespace is global
    // and `event_idempotency` is keyed on the aggregate with no project column.
    // So key "k" is genuinely FREE in project beta and simultaneously FATAL on
    // task t-1 — the two unique indexes disagree, and neither contains the
    // other. A pre-check that reads only the project-scoped one calls this
    // unused, and the append path's aggregate-scoped lookup then finds alpha's
    // row and answers beta with it: a command that never ran, reported applied,
    // carrying another project's payload and actor.
    let created = store
        .submit(&envelope_in(
            "alpha",
            "k",
            actor("kernel"),
            &task_in("alpha", "t-1"),
        ))
        .await;
    assert!(
        matches!(created, KernelResult::CommandApplied { .. }),
        "{created:?}"
    );

    let hijack = store
        .submit(&envelope_in(
            "beta",
            "k",
            actor("kernel"),
            &KernelCommand::TransitionTask {
                task_id: TaskId::new("t-1"),
                to: TaskState::Working,
                expected_version: 1,
            },
        ))
        .await;
    match &hijack {
        KernelResult::Error { code, .. } => {
            assert_eq!(*code, KernelErrorCode::IdempotencyConflict);
        }
        KernelResult::CommandApplied { events, .. } => panic!(
            "beta was answered with project {:?}'s event {:?}",
            events[0].project_id, events[0].event_id
        ),
        other => panic!("{other:?}"),
    }
    // Nothing moved, and beta was never handed alpha's log.
    assert_eq!(task_row(&store, "t-1").await, ("submitted".to_owned(), 1));
    assert_eq!(event_count(&store).await, 1);

    // The SAME create from another project is a conflict too — it is a
    // different request because it belongs to a different project's log.
    let twin = store
        .submit(&envelope_in(
            "beta",
            "k",
            actor("kernel"),
            &task_in("beta", "t-1"),
        ))
        .await;
    assert!(
        matches!(
            twin,
            KernelResult::Error {
                code: KernelErrorCode::IdempotencyConflict,
                ..
            }
        ),
        "{twin:?}"
    );

    // A FRESH key in beta gets past every idempotency check — and is still
    // refused, because the key was never the thing standing between beta and
    // alpha's task. t-1's first event says alpha, so alpha owns it.
    let fresh = store
        .submit(&envelope_in(
            "beta",
            "k2",
            actor("kernel"),
            &KernelCommand::TransitionTask {
                task_id: TaskId::new("t-1"),
                to: TaskState::Working,
                expected_version: 1,
            },
        ))
        .await;
    match &fresh {
        KernelResult::Error { code, message, .. } => {
            assert_eq!(*code, KernelErrorCode::Validation, "{fresh:?}");
            assert!(message.contains("alpha"), "{message}");
        }
        other => panic!("beta moved a task it does not own: {other:?}"),
    }
    // Still alpha's, still where alpha left it.
    assert_eq!(task_row(&store, "t-1").await, ("submitted".to_owned(), 1));
    assert_eq!(event_count(&store).await, 1);

    // Beta is not locked out of the kernel — only out of alpha's aggregate.
    let own = store
        .submit(&envelope_in(
            "beta",
            "k2",
            actor("kernel"),
            &task_in("beta", "t-2"),
        ))
        .await;
    assert!(
        matches!(own, KernelResult::CommandApplied { .. }),
        "{own:?}"
    );

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_create_whose_body_names_another_project_is_refused() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "bodyproject", 64).await;

    // Two fields describe the same task's project and they disagree. Landing
    // it would write a row saying one thing over a log saying the other.
    let split = store
        .submit(&envelope_in(
            "alpha",
            "k",
            actor("kernel"),
            &task_in("beta", "t-1"),
        ))
        .await;
    match &split {
        KernelResult::Error { code, message, .. } => {
            assert_eq!(*code, KernelErrorCode::Validation, "{split:?}");
            assert!(
                message.contains("beta") && message.contains("alpha"),
                "{message}"
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(event_count(&store).await, 0);

    // A body with no project of its own is not a disagreement.
    let quiet = store
        .submit(&envelope_in(
            "alpha",
            "k2",
            actor("kernel"),
            &KernelCommand::CreateTask {
                task_id: TaskId::new("t-2"),
                kind: None,
                title: None,
                spec_ref: None,
                project: None,
                priority: None,
                tracker_ref: None,
            },
        ))
        .await;
    assert!(
        matches!(quiet, KernelResult::CommandApplied { .. }),
        "{quiet:?}"
    );

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn the_envelopes_routing_and_cas_fields_are_honored_not_ignored() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "routing", 64).await;

    apply(&store, "task", task("t-1")).await;
    apply(
        &store,
        "wt",
        KernelCommand::RegisterWorktree {
            worktree_id: WorktreeId::new("wt-1"),
            repo: "gridwork".into(),
            path: "/w/kernel".into(),
            branch: "feature/kernel".into(),
            base_sha: None,
            lease_id: None,
        },
    )
    .await;

    let labelled = |key: &str,
                    ty: Option<&str>,
                    id: Option<&str>,
                    version: Option<u32>,
                    command: &KernelCommand| {
        let mut envelope = envelope(key, command);
        envelope.target_aggregate_type = ty.map(str::to_owned);
        envelope.target_aggregate_id = id.map(AggregateId::new);
        envelope.expected_version = version;
        envelope
    };
    let transition = KernelCommand::TransitionTask {
        task_id: TaskId::new("t-1"),
        to: TaskState::Working,
        expected_version: 1,
    };

    // A label that names another aggregate must not quietly mutate the one the
    // body names — the same reason from_envelope refuses a command_type that
    // disagrees with its body.
    for (key, ty, id) in [
        ("wrong-type", Some("attempt"), None),
        ("wrong-id", None, Some("t-2")),
    ] {
        let result = store
            .submit(&labelled(key, ty, id, None, &transition))
            .await;
        assert!(
            matches!(
                result,
                KernelResult::Error {
                    code: KernelErrorCode::Validation,
                    ..
                }
            ),
            "{key}: {result:?}"
        );
    }
    assert_eq!(task_row(&store, "t-1").await, ("submitted".to_owned(), 1));

    // For a command whose body carries no version, the envelope's field is the
    // only CAS a caller can express. Ignoring it makes an intended
    // compare-and-swap a silent last-write-wins.
    let update = KernelCommand::UpdateWorktree {
        worktree_id: WorktreeId::new("wt-1"),
        dirty: true,
        unpushed: false,
        base_sha: None,
    };
    let stale = store
        .submit(&labelled("wt-stale", None, None, Some(9), &update))
        .await;
    assert!(
        matches!(
            stale,
            KernelResult::Error {
                code: KernelErrorCode::StaleVersion,
                ..
            }
        ),
        "{stale:?}"
    );
    let dirty: bool = sqlx::query_scalar("SELECT dirty FROM gwk.worktree WHERE id = $1")
        .bind("wt-1")
        .fetch_one(store.pool())
        .await
        .expect("worktree row");
    assert!(!dirty, "the refused update was applied anyway");

    // Correctly labelled and correctly versioned goes through.
    let ok = store
        .submit(&labelled(
            "wt-ok",
            Some("worktree"),
            Some("wt-1"),
            Some(1),
            &update,
        ))
        .await;
    assert!(matches!(ok, KernelResult::CommandApplied { .. }), "{ok:?}");

    // A create asserts the aggregate has no events, so 0 is the only version an
    // envelope may claim for one.
    let claimed = store
        .submit(&labelled("t-3", None, None, Some(4), &task("t-3")))
        .await;
    assert!(
        matches!(
            claimed,
            KernelResult::Error {
                code: KernelErrorCode::StaleVersion,
                ..
            }
        ),
        "{claimed:?}"
    );

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_replayed_key_from_a_different_actor_does_not_answer_a_guarded_flip() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "replayguard", 64).await;

    apply(&store, "task", task("t-1")).await;
    apply(&store, "attempt", attempt("a-1", "t-1")).await;
    run_attempt(&store, "a-1", "run").await;

    let flip = KernelCommand::TransitionAttempt {
        attempt_id: AttemptId::new("a-1"),
        to: AttemptState::Blocked,
        expected_version: 4,
        receipt_id: Some(ReceiptId::new("r-1")),
    };
    let applied = store
        .submit(&envelope_as("flip", actor(LIVENESS_PRODUCER_KIND), &flip))
        .await;
    assert!(
        matches!(applied, KernelResult::CommandApplied { .. }),
        "{applied:?}"
    );

    // The replay short-circuit answers BEFORE transition::apply runs, and apply
    // is the only place the liveness-producer rule lives. So replaying the key
    // and the body under a different actor must not be answered as that actor's
    // own success — `gwk_domain::transition` says in as many words that an
    // unauthorized replay of a guarded flip is refused, not answered.
    let harvested = store
        .submit(&envelope_as("flip", actor("engine"), &flip))
        .await;
    match &harvested {
        KernelResult::Error { code, .. } => {
            assert_eq!(*code, KernelErrorCode::IdempotencyConflict);
        }
        other => panic!("a harvested key answered a guarded flip: {other:?}"),
    }

    // The producer's own retry is still stable — that is the whole point of the
    // key, and tightening the identity must not cost it.
    let retry = store
        .submit(&envelope_as("flip", actor(LIVENESS_PRODUCER_KIND), &flip))
        .await;
    assert!(
        matches!(retry, KernelResult::CommandApplied { .. }),
        "{retry:?}"
    );
    assert_eq!(attempt_row(&store, "a-1").await, ("blocked".to_owned(), 5));

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn the_refusals_are_typed_values_not_database_exceptions() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "refusals", 64).await;

    apply(&store, "task", task("t-1")).await;

    // An edge that is not in the table.
    let (code, _) = refuse(
        &store,
        "illegal",
        KernelCommand::TransitionTask {
            task_id: TaskId::new("t-1"),
            to: TaskState::Completed,
            expected_version: 1,
        },
    )
    .await;
    assert_eq!(code, KernelErrorCode::IllegalEdge);

    // A legal edge at the wrong version.
    let (code, message) = refuse(
        &store,
        "stale",
        KernelCommand::TransitionTask {
            task_id: TaskId::new("t-1"),
            to: TaskState::Working,
            expected_version: 7,
        },
    )
    .await;
    assert_eq!(code, KernelErrorCode::StaleVersion);
    assert!(message.contains("actual 1"), "{message}");

    // A create whose aggregate already exists: the CAS at version 0 is what
    // refuses it, so there is no separate existence check to disagree with.
    let (code, _) = refuse(&store, "again", task("t-1")).await;
    assert_eq!(code, KernelErrorCode::StaleVersion);

    // An update to something that was never created.
    let (code, _) = refuse(
        &store,
        "ghost",
        KernelCommand::TransitionTask {
            task_id: TaskId::new("nope"),
            to: TaskState::Working,
            expected_version: 1,
        },
    )
    .await;
    assert_eq!(code, KernelErrorCode::NotFound);

    // A bound the DATABASE would otherwise raise on: refused as a value, before
    // the log. This entry used to be "a command this kernel does not project
    // yet"; ingestion was the last one, and there is no such command now.
    let (code, message) = refuse(
        &store,
        "oversized",
        KernelCommand::IngestRecord {
            kind: gwk_domain::ingestion::IngestionKind::Memory,
            payload: serde_json::json!({ "blob": "x".repeat(INLINE_PAYLOAD_MAX_BYTES) }),
            payload_ref: None,
        },
    )
    .await;
    assert_eq!(code, KernelErrorCode::Validation);
    assert!(message.contains("payload_ref"), "{message}");

    // Every refusal above rolled back; only the create survived.
    assert_eq!(event_count(&store).await, 1);

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn the_blocked_flip_admits_only_the_liveness_producer_with_a_receipt() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "flip", 64).await;

    apply(&store, "task", task("t-1")).await;
    apply(&store, "attempt", attempt("a-1", "t-1")).await;
    run_attempt(&store, "a-1", "run").await;

    let flip = |receipt: Option<&str>| KernelCommand::TransitionAttempt {
        attempt_id: AttemptId::new("a-1"),
        to: AttemptState::Blocked,
        expected_version: 4,
        receipt_id: receipt.map(ReceiptId::new),
    };

    // running -> blocked IS a legal edge and the version IS right, so nothing
    // in the DDL would stop this — the actor rule lives only in the domain's
    // apply path, and this is the proof that the command path routes through it.
    let (code, _) = refuse(&store, "wrong-actor", flip(Some("r-1"))).await;
    assert_eq!(code, KernelErrorCode::Authority);

    // The right actor without a receipt is still refused.
    let no_receipt = store
        .submit(&envelope_as(
            "no-receipt",
            actor(LIVENESS_PRODUCER_KIND),
            &flip(None),
        ))
        .await;
    assert!(
        matches!(
            no_receipt,
            KernelResult::Error {
                code: KernelErrorCode::Authority,
                ..
            }
        ),
        "{no_receipt:?}"
    );

    // Both together apply.
    let ok = store
        .submit(&envelope_as(
            "flip",
            actor(LIVENESS_PRODUCER_KIND),
            &flip(Some("r-1")),
        ))
        .await;
    assert!(matches!(ok, KernelResult::CommandApplied { .. }), "{ok:?}");
    assert_eq!(attempt_row(&store, "a-1").await, ("blocked".to_owned(), 5));

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_rounds_bump_keeps_the_row_version_equal_to_the_logs() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "rounds", 64).await;

    apply(&store, "task", task("t-1")).await;
    apply(&store, "attempt", attempt("a-1", "t-1")).await;

    // A round is a fact about the attempt with no column of its own. It still
    // advances the row, because the row's version is the aggregate's — and a
    // client's `expected_version` has to mean the same number to the CAS on the
    // log and to the CAS on the projection.
    apply(
        &store,
        "round",
        KernelCommand::RecordRound {
            attempt_id: AttemptId::new("a-1"),
            round: 1,
            findings: RoundFindingSummary {
                total: 2,
                auto_fix: 1,
                ask_user: 1,
                no_op: 0,
            },
        },
    )
    .await;
    apply(
        &store,
        "finding",
        KernelCommand::RecordFinding {
            attempt_id: AttemptId::new("a-1"),
            round: 1,
            action: FindingAction::AutoFix,
            summary: "unbounded read limit".into(),
            subject_ref: None,
        },
    )
    .await;
    assert_eq!(attempt_row(&store, "a-1").await, ("queued".to_owned(), 3));

    let log_version: i64 = sqlx::query_scalar(
        "SELECT max(aggregate_version) FROM gwk.event \
         WHERE aggregate_type = 'attempt' AND aggregate_id = $1",
    )
    .bind("a-1")
    .fetch_one(store.pool())
    .await
    .expect("log version");
    assert_eq!(log_version, 3, "the row and the log disagree about version");

    // Which means a client holding the pre-round version is now honestly stale.
    let (code, _) = refuse(
        &store,
        "stale-after-round",
        KernelCommand::TransitionAttempt {
            attempt_id: AttemptId::new("a-1"),
            to: AttemptState::Leased,
            expected_version: 1,
            receipt_id: None,
        },
    )
    .await;
    assert_eq!(code, KernelErrorCode::StaleVersion);

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_checkpoint_seq_may_not_rewind_or_stand_still() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "checkpoint", 64).await;

    let checkpoint = |seq: u64| KernelCommand::WriteOrchestratorCheckpoint {
        checkpoint: OrchestratorCheckpoint {
            orchestrator_id: Some("orch-1".into()),
            seq: Seq::new(seq),
            native_session_ref: None,
            active_goal: None,
            active_step_ref: None,
            latest_command_ref: None,
            open_attempts: None,
            leases: None,
            pending_approvals: None,
            budget_cursor: None,
        },
    };

    apply(&store, "cp-5", checkpoint(5)).await;
    // A resume cursor, so both a rewind and a stand-still would let recovery
    // restart from a snapshot that has already been superseded.
    for (key, seq) in [("cp-4", 4u64), ("cp-5-again", 5)] {
        let (code, _) = refuse(&store, key, checkpoint(seq)).await;
        assert_eq!(code, KernelErrorCode::StaleVersion, "seq {seq}");
    }
    apply(&store, "cp-6", checkpoint(6)).await;

    let seq: String = sqlx::query_scalar(
        "SELECT seq::text FROM gwk.orchestrator_checkpoint WHERE orchestrator_id = $1",
    )
    .bind("orch-1")
    .fetch_one(store.pool())
    .await
    .expect("checkpoint seq");
    assert_eq!(seq, "6");

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn the_dispatch_node_gets_a_version_cas_without_an_edge_table() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "spawn", 64).await;

    apply(
        &store,
        "node",
        KernelCommand::RegisterDispatchNode {
            dispatch_node_id: DispatchNodeId::new("n-1"),
            parent_id: None,
            attempt_id: None,
            kind: "orchestrator".into(),
            label: None,
        },
    )
    .await;

    let transition = |to: &str, expected: u32| KernelCommand::TransitionDispatchNode {
        dispatch_node_id: DispatchNodeId::new("n-1"),
        to: to.to_owned(),
        expected_version: expected,
    };

    // The label is OPEN — the spawn tree has no seeded edges, so anything the
    // caller names is legal and only the version is checked.
    apply(&store, "n-running", transition("running", 1)).await;
    apply(
        &store,
        "n-odd",
        transition("whatever_the_engine_calls_it", 2),
    )
    .await;
    let (code, _) = refuse(&store, "n-stale", transition("finished", 2)).await;
    assert_eq!(code, KernelErrorCode::StaleVersion);

    let (state, version): (String, i64) = {
        let row = sqlx::query("SELECT state, version FROM gwk.dispatch_node WHERE id = $1")
            .bind("n-1")
            .fetch_one(store.pool())
            .await
            .expect("node row");
        (row.get("state"), row.get("version"))
    };
    assert_eq!(
        (state.as_str(), version),
        ("whatever_the_engine_calls_it", 3)
    );

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn the_lease_spine_carries_its_own_cas_to_release_and_expiry() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "lease", 64).await;

    let acquire = |id: &str| KernelCommand::AcquireLease {
        lease_id: LeaseId::new(id),
        mode: LeaseMode::Exclusive,
        holder: Some("a-1".into()),
        scope: Some("worktree".into()),
        repo: None,
        path: None,
        branch: None,
        base_sha: None,
        expires_at: Some(Timestamp::new("2026-07-28T01:00:00Z")),
    };

    apply(&store, "l-1", acquire("l-1")).await;
    apply(
        &store,
        "renew",
        KernelCommand::RenewLease {
            lease_id: LeaseId::new("l-1"),
            expected_version: 1,
            expires_at: Some(Timestamp::new("2026-07-28T02:00:00Z")),
        },
    )
    .await;
    // A heartbeat is a versioned write like any other — that is the lease
    // guard's rule, not a state-transition rule.
    let (state, version, expires): (String, i64, Option<String>) = {
        let row = sqlx::query(
            "SELECT state, version, to_json(expires_at) #>> '{}' AS expires \
             FROM gwk.lease WHERE id = $1",
        )
        .bind("l-1")
        .fetch_one(store.pool())
        .await
        .expect("lease row");
        (row.get("state"), row.get("version"), row.get("expires"))
    };
    assert_eq!((state.as_str(), version), ("held", 2));
    assert!(
        expires.is_some_and(|e| e.starts_with("2026-07-28T02:")),
        "the renewal did not move the deadline"
    );

    let (code, _) = refuse(
        &store,
        "release-stale",
        KernelCommand::ReleaseLease {
            lease_id: LeaseId::new("l-1"),
            expected_version: 1,
            disposition: Some("clean".into()),
        },
    )
    .await;
    assert_eq!(code, KernelErrorCode::StaleVersion);

    apply(
        &store,
        "release",
        KernelCommand::ReleaseLease {
            lease_id: LeaseId::new("l-1"),
            expected_version: 2,
            disposition: Some("clean".into()),
        },
    )
    .await;
    apply(&store, "l-2", acquire("l-2")).await;
    apply(
        &store,
        "expire",
        KernelCommand::ExpireLease {
            lease_id: LeaseId::new("l-2"),
            expected_version: 1,
        },
    )
    .await;

    let states: Vec<(String, String)> = sqlx::query("SELECT id, state FROM gwk.lease ORDER BY id")
        .fetch_all(store.pool())
        .await
        .expect("lease rows")
        .iter()
        .map(|row| (row.get("id"), row.get("state")))
        .collect();
    assert_eq!(
        states,
        vec![
            ("l-1".to_owned(), "released".to_owned()),
            ("l-2".to_owned(), "expired".to_owned()),
        ]
    );

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn an_outcome_records_without_erasing_what_an_earlier_one_established() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "outcome", 64).await;

    apply(&store, "task", task("t-1")).await;
    apply(&store, "attempt", attempt("a-1", "t-1")).await;

    apply(
        &store,
        "outcome-1",
        KernelCommand::RecordAttemptOutcome {
            attempt_id: AttemptId::new("a-1"),
            expected_version: 1,
            exit_code: Some(0),
            provider_terminal_event: Some("completed".into()),
            result_valid: Some(true),
            evidence_manifest_ref: None,
        },
    )
    .await;
    // Absent means "not specified" on the wire, so a later record that names
    // fewer fields must not erase the ones already established.
    apply(
        &store,
        "outcome-2",
        KernelCommand::RecordAttemptOutcome {
            attempt_id: AttemptId::new("a-1"),
            expected_version: 2,
            exit_code: None,
            provider_terminal_event: None,
            result_valid: None,
            evidence_manifest_ref: Some("blob://manifest".into()),
        },
    )
    .await;

    let row = sqlx::query(
        "SELECT exit_code, provider_terminal_event, result_valid, evidence_manifest_ref, version \
         FROM gwk.attempt WHERE id = $1",
    )
    .bind("a-1")
    .fetch_one(store.pool())
    .await
    .expect("attempt row");
    assert_eq!(row.get::<Option<i32>, _>("exit_code"), Some(0));
    assert_eq!(
        row.get::<Option<String>, _>("provider_terminal_event")
            .as_deref(),
        Some("completed")
    );
    assert_eq!(row.get::<Option<bool>, _>("result_valid"), Some(true));
    assert_eq!(
        row.get::<Option<String>, _>("evidence_manifest_ref")
            .as_deref(),
        Some("blob://manifest")
    );
    assert_eq!(row.get::<i64, _>("version"), 3);

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn an_engine_session_opens_and_closes_without_a_version_of_its_own() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "session", 64).await;

    apply(&store, "task", task("t-1")).await;
    apply(&store, "attempt", attempt("a-1", "t-1")).await;
    apply(
        &store,
        "open",
        KernelCommand::OpenEngineSession {
            engine_session_id: EngineSessionId::new("s-1"),
            attempt_id: AttemptId::new("a-1"),
            engine: EngineId::new("engine-a"),
            provider_session_ref: None,
        },
    )
    .await;

    // No CAS in the command, so none is invented — but closing something that
    // never opened is still a refusal, not a silent no-op.
    let (code, _) = refuse(
        &store,
        "close-ghost",
        KernelCommand::CloseEngineSession {
            engine_session_id: EngineSessionId::new("s-missing"),
        },
    )
    .await;
    assert_eq!(code, KernelErrorCode::NotFound);

    apply(
        &store,
        "close",
        KernelCommand::CloseEngineSession {
            engine_session_id: EngineSessionId::new("s-1"),
        },
    )
    .await;
    let ended: Option<String> = sqlx::query_scalar(
        "SELECT to_json(ended_at) #>> '{}' FROM gwk.engine_session WHERE id = $1",
    )
    .bind("s-1")
    .fetch_one(store.pool())
    .await
    .expect("session row");
    assert!(ended.is_some(), "the close did not stamp the session");

    drop_database(&maintenance, &name).await;
}
