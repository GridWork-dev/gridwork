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
//!    (the FSM-derived set [`crate::recover::uncertain_states`] names, not a
//!    literal here that could drift from it) — `TransitionAttempt`
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
//! # Two triggers, not one
//!
//! A lease that expired is one trigger. It is not the only way an attempt
//! stops being alive, and it was never the whole job: `worktree_lease_id` is
//! nullable on the wire and in the schema, so a dispatch that needs no
//! worktree — a read-only review, say — creates an attempt with no lease at
//! all. Nothing about that attempt's death produces a lease to expire, so the
//! arm above is blind to it by construction, and one that crashed while
//! `running` stayed `running` forever: the kernel went on claiming work was
//! in flight that nothing was performing.
//!
//! So the pass has a second trigger: an attempt in
//! one of the uncertain states except `blocked` (silence is evidence only
//! where silence is abnormal — see [`stale_state_names`]) that NO live lease
//! is backing and that has not produced a single event for [`STALE_AFTER`]
//! is marked `unknown` too. "No live lease" rather than "no lease" on purpose — it also catches
//! the attempt whose lease is already `expired` or `released`, which the
//! lease loop can never re-find (it only ever reads `state = 'held'`), and
//! the one whose `held` lease carries no `expires_at` at all, which neither
//! loop could otherwise reach: a lease that never expires is a declaration,
//! not proof of life. A consequence worth naming: `ReleaseLease` carries no
//! holder check, so releasing an attempt's lease — anyone's to do on this
//! same-uid socket — is also what strips it of this arm's protection.
//!
//! Both writes are per-row CAS, so a lost race on either one is not an
//! error — the next tick re-reads the current state and decides again. A
//! query failure is the one thing [`sweep_once_with`] propagates; everything
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

/// How long an attempt with no live lease may go without a single event
/// before the kernel stops claiming it runs.
///
/// The lease trigger is prompt because an elapsed TTL is proof of death. An
/// attempt no lease is backing offers no such proof, so silence is the only
/// signal there is — and `unknown` is terminal
/// ([`gwk_domain::fsm::AttemptState`] gives it no outgoing edge), so a false
/// positive is permanent. The value therefore has to exceed the longest
/// legitimate quiet stretch of a live lease-less dispatch, not merely the
/// sweep's own cadence. It is affordable because six commands refresh
/// `gwk.attempt.updated_at` — `TransitionAttempt`, `UpdateBudget`,
/// `RecordAttemptRuntime`, `RecordAttemptOutcome`, `RecordRound` and
/// `RecordFinding` — so an attempt that is doing anything at all advances it.
pub const STALE_AFTER: Duration = Duration::from_secs(3600);

/// The most rows one stale pass will act on.
///
/// The stale query runs inside the daemon's own sweep task every
/// [`SWEEP_INTERVAL`], and each hit costs a CAS command under the writer
/// lock — unbounded, a backlog would turn one tick into a writer-lock hog
/// that starves client commands for its whole length. Bounded, the pass
/// makes deterministic forward progress (`ORDER BY a.id`) and a backlog
/// drains across ticks at a rate the lock can absorb.
/// `ponytail:` the query is a seq scan (no index on `gwk.attempt.state` /
/// `updated_at`) — fine at the hundreds-of-rows table this estate carries;
/// a partial index via the migration chain is the upgrade if it ever grows.
const STALE_SWEEP_BATCH: i64 = 256;

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

/// One pass at the threshold production wires: [`STALE_AFTER`].
///
/// Every tick [`run`] takes goes through here, so the shipped value is this
/// function's, never a caller's.
pub async fn sweep_once(store: &PgEventStore) -> Result<SweepReport> {
    sweep_once_with(store, STALE_AFTER).await
}

/// One pass: expire every lease whose TTL elapsed, and mark `unknown` every
/// attempt it was backing that had not already reached a terminal or
/// not-yet-live state — then every attempt no live lease is backing that has
/// been silent for `stale_after`.
///
/// The threshold is a parameter only so a test can move it. A row's
/// `updated_at` is written from its event's `appended_at`, which is
/// Postgres's own `now()` at append, and `assert_transition` requires every
/// UPDATE to advance `version` by exactly one — so a fixture cannot be
/// backdated by SQL or by command, and the threshold is the only lever left.
/// Hidden for the same reason it is public: the integration tests need it,
/// and nothing else should call it — an arbitrary threshold is a "bury
/// everything now" lever, not a supported entry point.
#[doc(hidden)]
pub async fn sweep_once_with(store: &PgEventStore, stale_after: Duration) -> Result<SweepReport> {
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
        let result = store.submit(&envelope).await;
        let KernelResult::CommandApplied { .. } = result else {
            // Raced: renewed since the read above (the holder is alive) or
            // already expired by another tick — benign. A refusal is not, and
            // would repeat on every tick (see `not_buried`); the printed
            // result is what tells the two apart. Either way this lease's
            // attempts are not this pass's to touch.
            let detail: String = format!("{result:?}").chars().take(300).collect();
            eprintln!(
                "gwk-kernel: ttl_sweep did not expire lease {} (version {version}): {detail}",
                shown(&lease_id)
            );
            continue;
        };
        report.leases_expired.push(LeaseId::new(lease_id.clone()));
        sweep_dead_attempts(store, &lease_id, &issued_at, &mut report).await?;
    }

    sweep_stale_attempts(store, stale_after, &issued_at, &mut report).await?;

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
           AND a.state = ANY($2)",
    )
    .bind(lease_id)
    .bind(uncertain_state_names()?)
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
        let result = store.submit(&envelope).await;
        if let KernelResult::CommandApplied { .. } = result {
            report
                .attempts_marked_unknown
                .push(AttemptId::new(attempt_id));
        } else {
            not_buried("lease", &attempt_id, version, &result);
        }
    }
    Ok(())
}

/// The journal line for a burial that did not land.
///
/// A lost CAS is benign — the row moved on its own between the read and this
/// write — but an idempotency or storage refusal means this row's key can
/// never land, and its version will not advance while it sits stale, so the
/// same refusal would repeat silently on every tick forever. Printing the
/// result is what makes the two distinguishable from the journal; the wire
/// refuses client envelopes in the sweep's key namespace precisely so the
/// second kind cannot be manufactured.
fn not_buried(arm: &str, attempt_id: &str, version: u32, result: &KernelResult) {
    let detail: String = format!("{result:?}").chars().take(300).collect();
    eprintln!(
        "gwk-kernel: ttl_sweep ({arm} arm) did not bury attempt {} (version {version}): {detail}",
        shown(attempt_id)
    );
}

/// A client-minted id, bounded and `Debug`-escaped for the journal.
///
/// Ids arrive over the wire without charset or length validation, so a raw
/// interpolation would let one carry newlines or terminal control bytes into
/// the operator's journal as forged `gwk-kernel:` lines.
fn shown(id: &str) -> String {
    let truncated: String = id.chars().take(128).collect();
    format!("{truncated:?}")
}

/// Mark `unknown` every attempt that no live lease is backing and that has
/// gone `stale_after` without a single event.
///
/// The lease arm cannot reach these: an attempt created with
/// `worktree_lease_id` NULL has no lease whose expiry would trigger it, and
/// one whose lease already left `held` is equally unreachable, because the
/// lease loop only ever re-reads `state = 'held'`. The `NOT EXISTS` is one
/// predicate for every spelling of the same fact — nothing is proving this
/// attempt alive — and it demands `expires_at IS NOT NULL` because a `held`
/// lease with no TTL is a declaration nothing ever has to renew: counting it
/// as proof would leave its attempt unreachable by both arms forever, the
/// shipped defect in a third shape.
async fn sweep_stale_attempts(
    store: &PgEventStore,
    stale_after: Duration,
    issued_at: &Timestamp,
    report: &mut SweepReport,
) -> Result<()> {
    let rows: Vec<(String, i64, String, f64)> = sqlx::query_as(
        "SELECT a.id, a.version, e.project_id, \
                extract(epoch FROM now() - a.updated_at)::float8 \
         FROM gwk.attempt a \
         JOIN LATERAL ( \
             SELECT project_id FROM gwk.event \
             WHERE aggregate_type = 'attempt' AND aggregate_id = a.id \
             ORDER BY aggregate_version LIMIT 1 \
         ) e ON true \
         WHERE a.state = ANY($1) \
           AND a.updated_at < now() - make_interval(secs => $2::float8) \
           AND NOT EXISTS ( \
               SELECT 1 FROM gwk.lease l \
               WHERE l.id = a.worktree_lease_id AND l.state = 'held' \
                 AND l.expires_at IS NOT NULL \
           ) \
         ORDER BY a.id \
         LIMIT $3",
    )
    .bind(stale_state_names()?)
    .bind(stale_after.as_secs_f64())
    .bind(STALE_SWEEP_BATCH)
    .fetch_all(store.pool())
    .await
    .map_err(KernelError::Database)?;

    for (attempt_id, version, project_id, age_secs) in rows {
        let version = version_of(version)?;
        let command = KernelCommand::TransitionAttempt {
            attempt_id: AttemptId::new(attempt_id.clone()),
            to: AttemptState::Unknown,
            expected_version: version,
            receipt_id: None,
        };
        let envelope = kernel_envelope(
            &command,
            // A different prefix from the lease arm's, so the log itself says
            // which trigger buried the attempt.
            format!("ttl_sweep:attempt_stale:{attempt_id}:{version}"),
            &project_id,
            issued_at,
        );
        let result = store.submit(&envelope).await;
        if let KernelResult::CommandApplied { .. } = result {
            // `run` discards the report, so stderr is the operational trace
            // this leaves behind — burying an attempt is not routine, and an
            // operator reading the journal should see which ones and why.
            eprintln!(
                "gwk-kernel: ttl_sweep buried attempt {} (version {version}) \
                 after {age_secs:.0}s with no live lease backing it",
                shown(&attempt_id)
            );
            report
                .attempts_marked_unknown
                .push(AttemptId::new(attempt_id));
        } else {
            not_buried("stale", &attempt_id, version, &result);
        }
    }
    Ok(())
}

/// The FSM's uncertain states, in the error type this module speaks.
///
/// One derivation, shared with the recovery report that names the same rows —
/// a second literal here could drift from the transitions it selects for.
fn uncertain_state_names() -> Result<Vec<String>> {
    crate::recover::uncertain_states()
        .map_err(|e| KernelError::Schema(format!("uncertain states: {e}")))
}

/// The stale arm's own candidate set: [`uncertain_state_names`] minus
/// `blocked`.
///
/// The two arms hold different evidence. The lease arm has proof of death —
/// an elapsed TTL — so it acts on every state with an edge to `unknown`. The
/// stale arm has only silence, and `blocked` is the one state whose declared
/// meaning IS legitimate silence: the running <-> blocked flip is receipted
/// by the liveness producer ([`gwk_domain::transition`]), so a blocked
/// attempt sitting quiet is doing exactly what it said it would — and
/// `unknown` is terminal, so burying it on wall-clock silence would replace
/// the defect this arm closes with its irreversible mirror image. A blocked
/// attempt whose lease dies is still buried, by the lease arm, on proof.
/// Subtractive from the shared derivation on purpose: a future FSM edge
/// change flows through without a second literal to drift.
fn stale_state_names() -> Result<Vec<String>> {
    let blocked = crate::project::wire_str(&AttemptState::Blocked)
        .map_err(|e| KernelError::Schema(format!("blocked wire name: {e}")))?;
    Ok(uncertain_state_names()?
        .into_iter()
        .filter(|state| *state != blocked)
        .collect())
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The value `sweep_once` — and therefore every production tick — wires
    /// into [`sweep_once_with`]. The integration arms prove the threshold is
    /// READ; this proves the number production reads is a usable one.
    ///
    /// Both bounds are load-bearing in opposite directions. Too low and the
    /// sweep buries live work, because `unknown` is terminal and a lease-less
    /// dispatch can legitimately go a long time between events; the floor is
    /// a literal on purpose — anchored to [`SWEEP_INTERVAL`] it would move
    /// whenever the cadence did, and a guard whose two sides travel together
    /// cannot fail in the direction that matters. Too high and the defect
    /// this arm exists to close is simply back — an attempt nothing is
    /// running keeps reading `running` for longer than anyone will wait.
    #[test]
    fn the_stale_threshold_is_bounded() {
        assert!(
            STALE_AFTER >= Duration::from_secs(30 * 60),
            "STALE_AFTER ({STALE_AFTER:?}) must exceed the longest legitimate quiet \
             stretch of a live lease-less dispatch — `unknown` is terminal, so a \
             short threshold buries real work permanently"
        );
        assert!(
            STALE_AFTER <= Duration::from_secs(24 * 60 * 60),
            "STALE_AFTER ({STALE_AFTER:?}) must still drain within a day — \
             a threshold nothing reaches is the defect, not the fix"
        );
    }

    /// A source-level pin on the wiring itself: `sweep_once` must pass the
    /// owned const, verbatim. Behavioural coverage cannot reach this line —
    /// `updated_at` is written by the store on every command and cannot be
    /// backdated, so no test fixture can age an attempt past a real
    /// [`STALE_AFTER`] — and without this pin, rewiring `sweep_once` to any
    /// other duration leaves the whole suite green while the shipped defect
    /// (a threshold nothing reaches) returns in full.
    /// The stale arm may act on silence only where silence is evidence:
    /// `blocked` is a receipted declaration that silence is expected, and
    /// `unknown` is terminal — so it is excluded, and this pins the exact
    /// remainder the way `recover`'s own test pins the full set. Widening
    /// the arm back to the shared derivation reds here.
    #[test]
    fn the_stale_arm_excludes_what_declared_it_was_waiting() {
        assert_eq!(
            stale_state_names().expect("wire names"),
            ["starting", "running", "canceling"],
            "the stale set is the FSM's uncertain states minus `blocked`"
        );
    }

    #[test]
    fn sweep_once_wires_the_owned_const() {
        // concat!: `include_str!` sees this test's own source too, so a
        // one-piece needle would match its own string literal and the pin
        // could never fail.
        let wiring = concat!("sweep_once_with(store, STALE_", "AFTER).await");
        assert!(
            include_str!("ttl_sweep.rs").contains(wiring),
            "sweep_once must wire STALE_AFTER into sweep_once_with verbatim — \
             the production threshold is this file's, never a caller's. (This is \
             a source-text pin: a rename or reflow of the wiring line reds it \
             too — update the needle alongside, don't hunt a threshold bug.)"
        );
    }
}
