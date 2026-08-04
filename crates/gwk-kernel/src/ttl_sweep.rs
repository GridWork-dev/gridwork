//! The kernel-owned TTL sweep: what happens when a lease-holder stops
//! heartbeating.
//!
//! An engine host acquires [`gwk_domain::command::KernelCommand::AcquireLease`]
//! for the work it holds and proves it is still alive with
//! [`gwk_domain::command::KernelCommand::RenewLease`] — the shipped
//! heartbeat pattern this module builds on. Liveness must survive a host
//! CRASH, not just an orderly release, so the party that notices a stale
//! `expires_at` cannot be the host itself — it is the one thing a dead host
//! cannot report. It has to be the kernel, on its own clock, and it has to
//! say so through the same command path every other write uses: an internal
//! [`crate::store::PgEventStore::submit`] call, actor `kernel`, exactly the
//! shape a client's own [`gwk_domain::command::KernelCommand::ExpireLease`]
//! or `TransitionAttempt` would take. There is no side-channel `UPDATE`
//! here — a sweep tick IS a command submission, so it gets the CAS, the
//! event, and the projection write for free, and costs nothing extra to
//! replay.
//!
//! Two writes per dead lease, in order:
//!
//! 1. `ExpireLease` — the lease itself moves `held -> expired`.
//! 2. For every attempt this lease was backing
//!    ([`gwk_domain::entity::Attempt::worktree_lease_id`]) and that is still
//!    in a state with an edge to [`gwk_domain::fsm::AttemptState::Unknown`]
//!    (`starting`, `running`, `blocked`, `canceling`) — `TransitionAttempt`
//!    to `unknown`, the FSM's own answer to "an attempt whose real outcome
//!    cannot be determined". The attempt is chosen over `engine_session`
//!    because that table carries `started_at`/`ended_at` only, no state of
//!    its own; the attempt's `unknown` escape edge IS the observed-liveness
//!    state a session's death shows up as.
//!
//! Step 2 runs ONLY after step 1 actually applied. If `ExpireLease` lost its
//! CAS race — a heartbeat landed between this sweep's read and its write, or
//! a concurrent tick already expired it — the lease is not this tick's to
//! act on further: a renewed lease means the holder is alive, and marking
//! its attempt `unknown` under it would be a false liveness report.
//!
//! Both writes are per-row CAS, so a lost race on either one is not an
//! error — the next tick re-reads the current state and decides again. A
//! query failure is the one thing [`sweep_once`] propagates; everything
//! about an individual row racing is absorbed.

use std::sync::Arc;
use std::time::Duration;

use gwk_domain::command::KernelCommand;
use gwk_domain::envelope::{Actor, CommandEnvelope, ENVELOPE_SCHEMA_VERSION, Origin};
use gwk_domain::fsm::AttemptState;
use gwk_domain::ids::{AttemptId, CommandId, IdempotencyKey, LeaseId, ProjectId, Timestamp};
use gwk_domain::protocol::KernelResult;

use crate::error::{KernelError, Result};
use crate::store::PgEventStore;

/// How often the daemon looks for a lease whose TTL elapsed.
///
/// Independent of any lease's own `expires_at` (set by whoever acquired it):
/// this is only how promptly a dead holder is noticed, not how long a live
/// one is trusted. `ponytail:` a fixed interval rather than a configurable
/// one — nothing in the contract or the (not-yet-built) engine host demands
/// a different cadence yet; add a knob when one does.
pub const SWEEP_INTERVAL: Duration = Duration::from_secs(15);

/// What one sweep pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SweepReport {
    pub leases_expired: Vec<LeaseId>,
    pub attempts_marked_unknown: Vec<AttemptId>,
}

/// Sweep forever, on [`SWEEP_INTERVAL`], until the task is dropped.
///
/// A tick's own failure — a lost connection, a busy pool — heals on the next
/// one, the same posture [`crate::wire::subscribe::watch_events`] takes on
/// its notification listener: nothing in here is fatal to the daemon, and a
/// missed tick just means a dead lease is noticed [`SWEEP_INTERVAL`] later
/// than it could have been.
pub async fn run(store: Arc<PgEventStore>, interval: Duration) {
    loop {
        tokio::time::sleep(interval).await;
        let _ = sweep_once(&store).await;
    }
}

/// One pass: expire every lease whose TTL elapsed, and mark `unknown` every
/// attempt it was backing that had not already reached a terminal or
/// not-yet-live state.
pub async fn sweep_once(store: &PgEventStore) -> Result<SweepReport> {
    let mut report = SweepReport::default();
    // One instant for every command this pass issues — the same thing one
    // client request's `issued_at` would be, and cheaper than a database
    // round trip per command.
    let issued_at = db_now(store).await?;

    let expired: Vec<(String, i64, String)> = sqlx::query_as(
        "SELECT l.id, l.version, e.project_id \
         FROM gwk.lease l \
         JOIN LATERAL ( \
             SELECT project_id FROM gwk.event \
             WHERE aggregate_type = 'lease' AND aggregate_id = l.id \
             ORDER BY aggregate_version LIMIT 1 \
         ) e ON true \
         WHERE l.state = 'held' AND l.expires_at IS NOT NULL AND l.expires_at < now()",
    )
    .fetch_all(store.pool())
    .await
    .map_err(KernelError::Database)?;

    for (lease_id, version, project_id) in expired {
        let version = version_of(version)?;
        let command = KernelCommand::ExpireLease {
            lease_id: LeaseId::new(lease_id.clone()),
            expected_version: version,
        };
        let envelope = kernel_envelope(
            &command,
            format!("ttl_sweep:expire_lease:{lease_id}:{version}"),
            &project_id,
            &issued_at,
        );
        let KernelResult::CommandApplied { .. } = store.submit(&envelope).await else {
            // Raced: renewed since the read above (the holder is alive) or
            // already expired by another tick. Either way this lease's
            // attempts are not this pass's to touch.
            continue;
        };
        report.leases_expired.push(LeaseId::new(lease_id.clone()));
        sweep_dead_attempts(store, &lease_id, &issued_at, &mut report).await?;
    }

    Ok(report)
}

/// Mark `unknown` every attempt this now-expired lease was backing, unless
/// it already left the states with an edge there.
async fn sweep_dead_attempts(
    store: &PgEventStore,
    lease_id: &str,
    issued_at: &Timestamp,
    report: &mut SweepReport,
) -> Result<()> {
    let rows: Vec<(String, i64, String)> = sqlx::query_as(
        "SELECT a.id, a.version, e.project_id \
         FROM gwk.attempt a \
         JOIN LATERAL ( \
             SELECT project_id FROM gwk.event \
             WHERE aggregate_type = 'attempt' AND aggregate_id = a.id \
             ORDER BY aggregate_version LIMIT 1 \
         ) e ON true \
         WHERE a.worktree_lease_id = $1 \
           AND a.state IN ('starting', 'running', 'blocked', 'canceling')",
    )
    .bind(lease_id)
    .fetch_all(store.pool())
    .await
    .map_err(KernelError::Database)?;

    for (attempt_id, version, project_id) in rows {
        let version = version_of(version)?;
        let command = KernelCommand::TransitionAttempt {
            attempt_id: AttemptId::new(attempt_id.clone()),
            to: AttemptState::Unknown,
            expected_version: version,
            // Only the running <-> blocked flip demands one; every edge into
            // `unknown` is an unguarded escape (`transition::apply`).
            receipt_id: None,
        };
        let envelope = kernel_envelope(
            &command,
            format!("ttl_sweep:attempt_unknown:{attempt_id}:{version}"),
            &project_id,
            issued_at,
        );
        if let KernelResult::CommandApplied { .. } = store.submit(&envelope).await {
            report
                .attempts_marked_unknown
                .push(AttemptId::new(attempt_id));
        }
        // A lost CAS here means the attempt moved on its own between the read
        // above and this write (finished, was stopped, whatever it was doing
        // completed) — not a sweep failure, just stale by the time it landed.
    }
    Ok(())
}

/// Narrow a `bigint` version column to the `u32` the domain carries.
///
/// The DDL already `CHECK`s every version into `[1, 2^32-1]`, so a failure
/// here means the schema and the domain have drifted, not that this row is
/// unusual.
fn version_of(raw: i64) -> Result<u32> {
    u32::try_from(raw).map_err(|_| KernelError::Schema(format!("version {raw} out of u32 range")))
}

/// The database's own clock — not the process's. `RenewLease`/`ExpireLease`
/// compare `expires_at` against Postgres's `now()` too, and a sweep that
/// stamped its commands from a clock that could disagree with that
/// comparison would be describing an instant nothing else in the contract
/// uses.
async fn db_now(store: &PgEventStore) -> Result<Timestamp> {
    let text: String = sqlx::query_scalar("SELECT to_json(now()) #>> '{}'")
        .fetch_one(store.pool())
        .await
        .map_err(KernelError::Database)?;
    Ok(Timestamp::new(text))
}

/// A command envelope this process minted for itself, in the project the
/// target aggregate already belongs to — the same shape a client's own
/// envelope would take, actor `kernel` in place of `operator`.
fn kernel_envelope(
    command: &KernelCommand,
    key: String,
    project: &str,
    issued_at: &Timestamp,
) -> CommandEnvelope {
    CommandEnvelope {
        command_id: CommandId::new(format!("cmd-{key}")),
        project_id: ProjectId::new(project),
        command_type: command.command_type().to_owned(),
        schema_version: ENVELOPE_SCHEMA_VERSION,
        issued_at: issued_at.clone(),
        actor: Actor {
            kind: "kernel".to_owned(),
            id: None,
        },
        origin: Origin {
            system: "kernel".to_owned(),
            r#ref: None,
        },
        target_aggregate_type: None,
        target_aggregate_id: None,
        expected_version: None,
        idempotency_key: IdempotencyKey::new(key),
        causation_id: None,
        correlation_id: None,
        // Infallible for a command built here — every variant serializes,
        // and a null payload would be refused by the kernel anyway (`gw`'s
        // own envelope builder makes the same call).
        payload: serde_json::to_value(command).unwrap_or(serde_json::Value::Null),
    }
}
