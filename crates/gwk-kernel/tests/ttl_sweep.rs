//! Certifies the kernel-owned TTL sweep against a real server: a lease-holder
//! that stopped heartbeating gets its lease expired and the attempt it was
//! backing flipped to `unknown` — and a holder that is still heartbeating is
//! left alone.
//!
//! `#[ignore]` because it needs a server — see `tests/common/mod.rs`.

mod common;

use common::{apply, drop_database, fresh_store, maintenance_pool, state_row, task};
use gwk_domain::command::KernelCommand;
use gwk_domain::fsm::{AttemptState, LeaseMode};
use gwk_domain::ids::{AttemptId, EngineId, LeaseId, TaskId, Timestamp};
use gwk_kernel::store::PgEventStore;
use gwk_kernel::ttl_sweep::sweep_once;

fn attempt(id: &str, task_id: &str, lease_id: &str) -> KernelCommand {
    KernelCommand::CreateAttempt {
        attempt_id: AttemptId::new(id),
        task_id: TaskId::new(task_id),
        engine: EngineId::new("engine-a"),
        capability: None,
        role: None,
        model_lane: None,
        permission_profile: None,
        worktree_lease_id: Some(LeaseId::new(lease_id)),
        base_sha: None,
        budget: None,
    }
}

fn lease(id: &str, holder: &str, expires_at: &str) -> KernelCommand {
    KernelCommand::AcquireLease {
        lease_id: LeaseId::new(id),
        mode: LeaseMode::Exclusive,
        holder: Some(holder.to_owned()),
        scope: Some("engine_session".into()),
        repo: Some("gridwork".into()),
        path: Some(format!("/w/{id}")),
        branch: Some("feature/x".into()),
        base_sha: None,
        expires_at: Some(Timestamp::new(expires_at)),
    }
}

/// Queued -> leased -> starting -> running: the states before this an
/// engine session is not yet the sweep's business, and the state it is most
/// realistically caught in when its host disappears mid-execution.
async fn drive_to_running(store: &PgEventStore, attempt_id: &str) {
    for (key, to, expected_version) in [
        ("leased", AttemptState::Leased, 1),
        ("starting", AttemptState::Starting, 2),
        ("running", AttemptState::Running, 3),
    ] {
        apply(
            store,
            &format!("{attempt_id}-{key}"),
            KernelCommand::TransitionAttempt {
                attempt_id: AttemptId::new(attempt_id),
                to,
                expected_version,
                receipt_id: None,
            },
        )
        .await;
    }
}

async fn lease_state(store: &PgEventStore, id: &str) -> (String, i64) {
    state_row(
        store,
        "SELECT state, version FROM gwk.lease WHERE id = $1",
        id,
    )
    .await
}

async fn attempt_state(store: &PgEventStore, id: &str) -> (String, i64) {
    state_row(
        store,
        "SELECT state, version FROM gwk.attempt WHERE id = $1",
        id,
    )
    .await
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_dead_holders_attempt_goes_unknown_and_a_live_ones_does_not() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "ttl_sweep", 8).await;

    apply(&store, "task", task("t-1")).await;

    // The seeded negative: a lease whose TTL elapsed before this sweep ever
    // ran — no renewal is coming, because the host that would send one is
    // gone. Backing a `running` attempt, the state a crash mid-execution
    // leaves behind.
    apply(
        &store,
        "lease-dead",
        lease("l-dead", "host-dead", "2000-01-01T00:00:00Z"),
    )
    .await;
    apply(&store, "attempt-dead", attempt("a-dead", "t-1", "l-dead")).await;
    drive_to_running(&store, "a-dead").await;

    // The control: an otherwise identical lease and attempt, except its TTL
    // is nowhere near elapsed — the heartbeating case the sweep must not
    // touch.
    apply(
        &store,
        "lease-alive",
        lease("l-alive", "host-alive", "2099-01-01T00:00:00Z"),
    )
    .await;
    apply(
        &store,
        "attempt-alive",
        attempt("a-alive", "t-1", "l-alive"),
    )
    .await;
    drive_to_running(&store, "a-alive").await;

    let report = sweep_once(&store).await.expect("sweep");
    assert_eq!(report.leases_expired, vec![LeaseId::new("l-dead")]);
    assert_eq!(
        report.attempts_marked_unknown,
        vec![AttemptId::new("a-dead")]
    );

    let (state, version) = lease_state(&store, "l-dead").await;
    assert_eq!(state, "expired");
    assert_eq!(version, 2); // acquired at 1, expired at 2

    let (state, version) = attempt_state(&store, "a-dead").await;
    assert_eq!(state, "unknown");
    // created(1) -> leased(2) -> starting(3) -> running(4) -> unknown(5)
    assert_eq!(version, 5);

    let (state, version) = lease_state(&store, "l-alive").await;
    assert_eq!(state, "held");
    assert_eq!(version, 1); // untouched: never renewed, never expired

    let (state, version) = attempt_state(&store, "a-alive").await;
    assert_eq!(state, "running");
    assert_eq!(version, 4); // untouched by the sweep

    // Idempotent in the way that matters: nothing left to expire or flip, so
    // a second pass finds nothing.
    let report = sweep_once(&store).await.expect("sweep again");
    assert!(report.leases_expired.is_empty());
    assert!(report.attempts_marked_unknown.is_empty());

    drop_database(&maintenance, &name).await;
}
