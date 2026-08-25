//! Certifies the kernel-owned TTL sweep against a real server: a lease-holder
//! that stopped heartbeating gets its lease expired and the attempt it was
//! backing flipped to `unknown` — and a holder that is still heartbeating is
//! left alone. Then the second trigger: an attempt no live lease is backing
//! ages out on its own, and one a held lease is backing does not.
//!
//! `#[ignore]` because it needs a server — see `tests/common/mod.rs`.

mod common;

use std::time::Duration;

use common::{
    actor, apply, apply_as, drop_database, fresh_store, maintenance_pool, state_row, task,
};
use gwk_domain::command::KernelCommand;
use gwk_domain::fsm::{AttemptState, LeaseMode};
use gwk_domain::ids::{AttemptId, EngineId, LeaseId, ReceiptId, TaskId, Timestamp};
use gwk_kernel::store::PgEventStore;
use gwk_kernel::ttl_sweep::{sweep_once, sweep_once_with};

/// `lease_id: None` is the lease-less dispatch the wire allows and the schema
/// stores as SQL NULL — a read-only review that needs no worktree.
fn attempt(id: &str, task_id: &str, lease_id: Option<&str>) -> KernelCommand {
    KernelCommand::CreateAttempt {
        attempt_id: AttemptId::new(id),
        task_id: TaskId::new(task_id),
        engine: EngineId::new("engine-a"),
        capability: None,
        role: None,
        model_lane: None,
        permission_profile: None,
        worktree_lease_id: lease_id.map(LeaseId::new),
        base_sha: None,
        budget: None,
    }
}

/// `expires_at: None` is the TTL-less acquisition the wire allows — a lease
/// nothing ever has to renew, which is why the sweep must not read one as
/// proof of life.
fn lease(id: &str, holder: &str, expires_at: Option<&str>) -> KernelCommand {
    KernelCommand::AcquireLease {
        lease_id: LeaseId::new(id),
        mode: LeaseMode::Exclusive,
        holder: Some(holder.to_owned()),
        scope: Some("engine_session".into()),
        repo: Some("gridwork".into()),
        path: Some(format!("/w/{id}")),
        branch: Some("feature/x".into()),
        base_sha: None,
        expires_at: expires_at.map(Timestamp::new),
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
        lease("l-dead", "host-dead", Some("2000-01-01T00:00:00Z")),
    )
    .await;
    apply(
        &store,
        "attempt-dead",
        attempt("a-dead", "t-1", Some("l-dead")),
    )
    .await;
    drive_to_running(&store, "a-dead").await;

    // The control: an otherwise identical lease and attempt, except its TTL
    // is nowhere near elapsed — the heartbeating case the sweep must not
    // touch.
    apply(
        &store,
        "lease-alive",
        lease("l-alive", "host-alive", Some("2099-01-01T00:00:00Z")),
    )
    .await;
    apply(
        &store,
        "attempt-alive",
        attempt("a-alive", "t-1", Some("l-alive")),
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

/// The second trigger, and the defect that made it necessary: an attempt
/// created with no worktree lease at all is unreachable from the lease arm
/// above — there is no lease of its own to expire — so before this arm existed
/// it read `running` forever with nothing performing it.
///
/// Three arms off one fixture, so "nothing happened" can never be a fixture
/// that would not have qualified anyway: the same three rows are swept twice,
/// once under the shipped threshold and once under a zero one.
#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_lease_less_attempt_ages_out_and_a_leased_or_fresh_one_does_not() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "ttl_sweep_stale", 8).await;

    apply(&store, "task", task("t-1")).await;

    // The production shape of the bug: worktree_lease_id NULL, driven to
    // `running`, then silent.
    apply(&store, "attempt-null", attempt("a-null", "t-1", None)).await;
    drive_to_running(&store, "a-null").await;

    // The control: the same state, but a lease that is held and nowhere near
    // expiring — something IS proving this one alive, so no amount of silence
    // may bury it.
    apply(
        &store,
        "lease-held",
        lease("l-held", "host-held", Some("2099-01-01T00:00:00Z")),
    )
    .await;
    apply(
        &store,
        "attempt-held",
        attempt("a-held", "t-1", Some("l-held")),
    )
    .await;
    drive_to_running(&store, "a-held").await;

    // The third shape of the same defect: a lease that is `held` but was
    // acquired with no `expires_at` at all. Nothing ever has to renew it, so
    // it proves nothing — the lease arm can never select it (no TTL to
    // elapse), and were it counted as live, this attempt would be
    // unreachable by both arms forever.
    apply(&store, "lease-nottl", lease("l-nottl", "host-nottl", None)).await;
    apply(
        &store,
        "attempt-nottl",
        attempt("a-nottl", "t-1", Some("l-nottl")),
    )
    .await;
    drive_to_running(&store, "a-nottl").await;

    // The declared-waiting negative: lease-less, but flipped to `blocked` —
    // the receipted transition only the liveness producer may write. This is
    // not silence the sweep may read as death; it is a receipt that silence
    // is expected, so no threshold may bury it.
    apply(&store, "attempt-blocked", attempt("a-blocked", "t-1", None)).await;
    drive_to_running(&store, "a-blocked").await;
    apply_as(
        &store,
        "a-blocked-flip",
        actor("liveness_producer"),
        KernelCommand::TransitionAttempt {
            attempt_id: AttemptId::new("a-blocked"),
            to: AttemptState::Blocked,
            expected_version: 4,
            receipt_id: Some(ReceiptId::new("r-blocked")),
        },
    )
    .await;

    // Arm 1: under an hour-long threshold, a row created seconds ago has not
    // been silent long enough for anything. The threshold is read, not
    // ignored.
    let report = sweep_once_with(&store, Duration::from_secs(3600))
        .await
        .expect("sweep under the threshold");
    assert!(
        report.attempts_marked_unknown.is_empty(),
        "nothing has been silent for an hour yet, got {:?}",
        report.attempts_marked_unknown
    );
    assert_eq!(attempt_state(&store, "a-null").await, ("running".into(), 4));
    assert_eq!(
        attempt_state(&store, "a-nottl").await,
        ("running".into(), 4)
    );

    // Arm 2: with the threshold at zero the lease-less one is buried, through
    // the ordinary command path — created(1) -> leased(2) -> starting(3) ->
    // running(4) -> unknown(5), the same ladder the lease arm produces. The
    // one whose held lease has no TTL is buried by the same pass: a lease
    // nothing ever has to renew is a declaration, not proof of life.
    let report = sweep_once_with(&store, Duration::ZERO)
        .await
        .expect("sweep at zero");
    assert_eq!(
        report.attempts_marked_unknown,
        vec![AttemptId::new("a-nottl"), AttemptId::new("a-null")]
    );
    assert_eq!(attempt_state(&store, "a-null").await, ("unknown".into(), 5));
    assert_eq!(
        attempt_state(&store, "a-nottl").await,
        ("unknown".into(), 5)
    );
    // Its lease is discounted as proof, never expired — it has no TTL for the
    // lease arm to act on, and the stale arm touches attempts only.
    assert_eq!(lease_state(&store, "l-nottl").await, ("held".into(), 1));
    // The declared-waiting one survived the same zero-threshold pass: the
    // stale arm's candidate set excludes `blocked` by derivation, so even
    // infinite silence is not evidence against it.
    assert_eq!(
        attempt_state(&store, "a-blocked").await,
        ("blocked".into(), 5)
    );

    // Arm 3: the same pass left the attempt with a live holder alone. Nothing
    // else in the sweep could have touched it — its lease expires in 2099, so
    // the expired-lease query never selects it.
    assert_eq!(attempt_state(&store, "a-held").await, ("running".into(), 4));
    assert_eq!(
        lease_state(&store, "l-held").await,
        ("held".into(), 1),
        "the live lease is untouched"
    );

    drop_database(&maintenance, &name).await;
}
