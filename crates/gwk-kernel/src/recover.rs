//! Recovery: what a restart is allowed to CLAIM about the projections.
//!
//! The projections are written by [`apply_event`] inside the very transaction
//! that appends the events, so a committed log and its projections cannot
//! disagree by construction. Recovery exists for the cases where something
//! outside that transaction went wrong — a restore that brought back a log
//! without its projections, storage corruption, a hand-edited row — and its job
//! is to say which of those it can rule out, honestly, rather than to assert
//! more than it checked.
//!
//! # A checkpoint cannot be restored, and that is the schema's decision
//!
//! The contract schema forbids it, twice, with `ENABLE ALWAYS` triggers that no
//! privilege and no `session_replication_role` can step around:
//!
//! * `<table>_born_initial` — a row must be born in its INITIAL state at
//!   version 1. A checkpoint's rows are at whatever state they reached, so
//!   loading them raises.
//! * `<table>_no_truncate` — the statement-level partner of the delete guard,
//!   there precisely to stop a TRUNCATE-then-reinsert walk-around.
//!
//! So a checkpoint is a VERIFICATION artifact, not a recovery shortcut: it
//! proves what the projections hashed to at one sequence, and the only way to
//! rebuild them is to replay the log from the beginning through the same
//! `apply_event` that wrote them. This is a narrower job than the plan first
//! assumed, and the narrowing is load-bearing — the guards are what make a
//! projection row unforgeable by anything but the log, and buying a faster
//! restart with a hole in them would be a bad trade.
//!
//! # What each verdict is worth
//!
//! [`Verdict::Verified`] is the strong one and needs a checkpoint AT the
//! watermark, which a clean shutdown produces by taking one last snapshot.
//! After a crash the newest checkpoint is behind the watermark and there is no
//! way to reconstruct the expected hash without replaying, so recovery returns
//! [`Verdict::Unverified`] and says so instead of implying a check it did not
//! run. That is not a degraded kernel — the tail is still transactionally
//! coupled to its events — it is an honest statement about what was proved.

use gwk_domain::blob::{BLOB_CHUNK_BYTES, BlobAddress};
use gwk_domain::checkpoint::{CHECKPOINT_SCHEMA_VERSION, Checkpoint};
use gwk_domain::fsm::{AttemptState, StateMachine};
use gwk_domain::ids::{ByteCount, Seq};
use gwk_domain::port::BlobStore;
use gwk_domain::protocol::ProjectionRecord;
use sqlx::{PgConnection, PgPool, Row};

use crate::blob::store::PgBlobStore;
use crate::checkpoint::{RECORDS_MEDIA_TYPE, checkpoints, derived_records, projection_hash};
use crate::epoch::{GENESIS_EVENT_TYPE, KERNEL_AGGREGATE};
use crate::numeric::from_numeric_text;
use crate::project::{Refusal, apply_event, wire_str};
use crate::store::{PgEventStore, read_page};

/// Events per replay page.
///
/// Small enough that a 100,000-event replay never holds more than a page of
/// envelopes at once, large enough that the round trip is not the cost.
const REPLAY_PAGE: usize = 1_000;

/// What recovery was able to prove.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// A valid checkpoint sat exactly at the watermark and the live
    /// projections hashed to its recorded hash. The strongest statement
    /// available, and what a clean shutdown sets up.
    Verified { anchor: Seq },
    /// The projections were empty and the log was replayed into them. The
    /// state was BUILT here, so there is nothing to compare it against beyond
    /// the checkpoint chain, which is checked when one lands at the watermark.
    Replayed { events: u64 },
    /// Nothing was proved, and the reason is named. The projections may be
    /// perfectly correct — they usually are — but this run did not establish
    /// it.
    Unverified { reason: String },
    /// The live projections did NOT hash to what the log says they should.
    /// Readiness must be refused: serving is the one thing that must not
    /// happen next.
    Diverged { expected: String, found: String },
}

/// What a restart found, and what it is entitled to claim.
#[derive(Debug, Clone)]
pub struct RecoveryReport {
    pub watermark: Option<Seq>,
    /// The live projections' hash, always computed — it is the cheap half of
    /// every verdict and the thing an operator will want in a bug report.
    pub live_hash: String,
    pub verdict: Verdict,
    /// Checkpoints the ladder walked PAST, each with why it was rejected.
    /// Surfaced rather than swallowed: a checkpoint failing validation is a
    /// storage problem that will keep happening, and silence lets it.
    pub rejected: Vec<(Seq, String)>,
    /// Attempts a restart left in a state the FSM says can end in `unknown`.
    ///
    /// Reported here, never acted on by `recover` itself — but no longer
    /// inert: within [`crate::ttl_sweep::STALE_AFTER`] the sweep transitions
    /// every one of these that no live lease is backing to terminal
    /// `unknown`. Same derivation, different actor. See
    /// [`PgEventStore::recover`].
    pub uncertain: Vec<String>,
}

impl RecoveryReport {
    /// Whether the kernel may serve. Only a proven divergence blocks it —
    /// "unverified" is a statement about this run, not an accusation.
    pub fn ready(&self) -> bool {
        !matches!(self.verdict, Verdict::Diverged { .. })
    }
}

/// The result of an operator-directed rebuild into a scratch database.
///
/// It never swaps anything. Replacing the live projections with a rebuilt set
/// is an operator act with its own downtime and its own blast radius, and a
/// function that did it as a side effect of a comparison would be a trap.
#[derive(Debug, Clone)]
pub struct RebuildReport {
    pub through_sequence: Option<Seq>,
    pub live_hash: String,
    pub rebuilt_hash: String,
    pub agrees: bool,
}

/// One replay's outputs, all read under a single snapshot of the source log.
struct Replayed {
    events: u64,
    watermark: Option<Seq>,
    live_hash: String,
    rebuilt_hash: String,
}

impl PgEventStore {
    /// Establish what a restart can prove about the projections.
    ///
    /// Writes nothing except on the cold path, where the projections are empty
    /// and the log is replayed into them through `apply_event` — the same
    /// writer that would have filled them originally.
    ///
    /// It never appends an event. An attempt that was running when the kernel
    /// died has an outcome the kernel did not observe, and `unknown` is a
    /// CLAIM about that outcome; minting one here would put a fact into the log
    /// that nothing witnessed, and would bury an attempt whose engine is in
    /// fact still alive. They are reported instead, and the transition stays an
    /// ordinary command with an actor behind it.
    pub async fn recover(&self) -> Result<RecoveryReport, Refusal> {
        let mut read = self
            .pool()
            .begin()
            .await
            .map_err(|e| Refusal::storage(format!("begin recovery read: {e}")))?;
        // One snapshot for the watermark, the live hash and the checkpoint
        // chain: a verdict assembled from three different moments would be a
        // verdict about a database that never existed.
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *read)
            .await
            .map_err(|e| Refusal::storage(format!("pin the recovery snapshot: {e}")))?;

        let watermark = watermark_of(&mut read).await?;
        let live = derived_records(&mut read).await?;
        let live_hash = projection_hash(&live);
        let (anchor, rejected) = newest_valid(&mut read, self.blobs(), watermark).await?;
        let cold = live.is_empty();
        read.rollback()
            .await
            .map_err(|e| Refusal::storage(format!("close the recovery read: {e}")))?;

        let verdict = match (watermark, cold) {
            // Nothing has ever been appended. Projections that are also empty
            // agree with that trivially; projections that are NOT are rows with
            // no log behind them, which is exactly the forgery the guards exist
            // to prevent and must not be served.
            (None, true) => Verdict::Unverified {
                reason: "the log is empty".to_owned(),
            },
            (None, false) => Verdict::Diverged {
                expected: projection_hash(&[]),
                found: live_hash.clone(),
            },
            // Cold: a log with no projections. Replay is the ONLY way to build
            // them, checkpoint or not.
            (Some(_), true) => {
                let built = self.replay_into_live().await?;
                match anchor
                    .as_ref()
                    .filter(|cp| Some(cp.through_sequence) == built.watermark)
                {
                    Some(cp) if cp.projection_hash != built.rebuilt_hash => Verdict::Diverged {
                        expected: cp.projection_hash.clone(),
                        found: built.rebuilt_hash,
                    },
                    _ => Verdict::Replayed {
                        events: built.events,
                    },
                }
            }
            (Some(mark), false) => match &anchor {
                Some(cp) if cp.through_sequence == mark => {
                    if cp.projection_hash == live_hash {
                        Verdict::Verified {
                            anchor: cp.through_sequence,
                        }
                    } else {
                        Verdict::Diverged {
                            expected: cp.projection_hash.clone(),
                            found: live_hash.clone(),
                        }
                    }
                }
                Some(cp) => Verdict::Unverified {
                    reason: format!(
                        "the newest valid checkpoint is at {}, and the log runs to {} — \
                         the projections cannot be re-derived in place to compare",
                        cp.through_sequence.value(),
                        mark.value()
                    ),
                },
                None if self.blobs().is_none() => Verdict::Unverified {
                    reason: "no blob store is attached, so no checkpoint can be read".to_owned(),
                },
                None => Verdict::Unverified {
                    reason: "no valid checkpoint".to_owned(),
                },
            },
        };

        // Read last: on the cold path the replay above is what put the attempts
        // there at all.
        let mut conn = self
            .pool()
            .acquire()
            .await
            .map_err(|e| Refusal::storage(format!("acquire: {e}")))?;
        let uncertain = uncertain_attempts(&mut conn).await?;

        Ok(RecoveryReport {
            watermark,
            live_hash,
            verdict,
            rejected,
            uncertain,
        })
    }

    /// Replay the whole log into an EMPTY scratch database and report whether
    /// it agrees with the live projections.
    ///
    /// This is the only full verification available once the log has moved past
    /// its newest checkpoint, and the only place a rebuild can happen at all,
    /// since the live tables refuse to be reset. It reads the live log, writes
    /// only to `scratch`, and leaves the decision to replace anything with the
    /// operator.
    pub async fn rebuild_into(&self, scratch: &PgPool) -> Result<RebuildReport, Refusal> {
        let mut tx = scratch
            .begin()
            .await
            .map_err(|e| Refusal::storage(format!("begin scratch rebuild: {e}")))?;
        // A rebuild INSERTs; against a scratch that already holds rows it would
        // either collide or, worse, quietly build a mixture and compare that.
        if !derived_records(&mut tx).await?.is_empty() {
            return Err(Refusal::validation(
                "the scratch database already holds projections — a rebuild needs an empty one",
            ));
        }
        let built = replay(self.pool(), &mut tx).await?;
        // Committed so the operator can inspect the rebuilt rows, diff them
        // against live, and decide. Nothing here acts on the answer.
        tx.commit()
            .await
            .map_err(|e| Refusal::storage(format!("commit scratch rebuild: {e}")))?;
        Ok(RebuildReport {
            through_sequence: built.watermark,
            live_hash: built.live_hash.clone(),
            agrees: built.live_hash == built.rebuilt_hash,
            rebuilt_hash: built.rebuilt_hash,
        })
    }

    /// Replay the log into this store's own empty projection tables.
    async fn replay_into_live(&self) -> Result<Replayed, Refusal> {
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| Refusal::storage(format!("begin cold replay: {e}")))?;
        let built = replay(self.pool(), &mut tx).await?;
        tx.commit()
            .await
            .map_err(|e| Refusal::storage(format!("commit cold replay: {e}")))?;
        Ok(built)
    }
}

/// Replay every event in `source`'s log into `target`'s projection tables.
///
/// The log is streamed from a repeatable-read snapshot held for the whole
/// replay, so the watermark, the live hash and every page describe one fixed
/// database. Without that a concurrent append would land in the rebuilt set but
/// not the live hash it is about to be compared against, and report a
/// divergence that is really just a race.
async fn replay(source: &PgPool, target: &mut PgConnection) -> Result<Replayed, Refusal> {
    let mut src = source
        .begin()
        .await
        .map_err(|e| Refusal::storage(format!("begin replay source: {e}")))?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *src)
        .await
        .map_err(|e| Refusal::storage(format!("pin the replay snapshot: {e}")))?;

    let watermark = watermark_of(&mut src).await?;
    let live_hash = projection_hash(&derived_records(&mut src).await?);

    let mut cursor = None;
    let mut events = 0u64;
    loop {
        let page = read_page(&mut *src, cursor, REPLAY_PAGE)
            .await
            .map_err(|e| Refusal::storage(format!("read the log: {e}")))?;
        if page.is_empty() {
            break;
        }
        for event in &page {
            // Genesis is the one event with no command behind it — "the epoch
            // boundary is the log itself", as `epoch` puts it, and nothing
            // issued it. It has no projection to write and its payload is not a
            // `KernelCommand`, so handing it to `apply_event` would fail every
            // replay on the first event of every log.
            if event.aggregate_type == KERNEL_AGGREGATE && event.event_type == GENESIS_EVENT_TYPE {
                continue;
            }
            apply_event(target, event).await?;
            events += 1;
        }
        cursor = page.last().map(|e| e.global_sequence);
        if page.len() < REPLAY_PAGE {
            break;
        }
    }

    let rebuilt_hash = projection_hash(&derived_records(target).await?);
    src.rollback()
        .await
        .map_err(|e| Refusal::storage(format!("close the replay source: {e}")))?;

    Ok(Replayed {
        events,
        watermark,
        live_hash,
        rebuilt_hash,
    })
}

/// The newest checkpoint that survives validation, plus every one walked past.
///
/// The ladder is the whole point: one unreadable checkpoint must not cost the
/// kernel every older one, so a rejection is recorded and the walk continues.
async fn newest_valid(
    conn: &mut PgConnection,
    blobs: Option<&PgBlobStore>,
    watermark: Option<Seq>,
) -> Result<(Option<Checkpoint>, Vec<(Seq, String)>), Refusal> {
    let Some(blobs) = blobs else {
        return Ok((None, Vec::new()));
    };
    let mut rejected = Vec::new();
    for checkpoint in checkpoints(conn).await? {
        match validate(blobs, &checkpoint, watermark).await {
            Ok(()) => return Ok((Some(checkpoint), rejected)),
            Err(reason) => rejected.push((checkpoint.through_sequence, reason)),
        }
    }
    Ok((None, rejected))
}

/// Whether a checkpoint still describes something.
///
/// Every check here has a failure that was actually seen or is actually
/// possible: a records blob swept or shredded out from under the row, a
/// truncated container, a checkpoint from a log that was later restored to an
/// earlier point. `Err` carries the reason so the caller can report it rather
/// than log a bare "invalid".
async fn validate(
    blobs: &PgBlobStore,
    checkpoint: &Checkpoint,
    watermark: Option<Seq>,
) -> Result<(), String> {
    if checkpoint.schema_version != CHECKPOINT_SCHEMA_VERSION {
        return Err(format!(
            "checkpoint schema {} is not {CHECKPOINT_SCHEMA_VERSION}",
            checkpoint.schema_version
        ));
    }
    // A checkpoint past the watermark describes a log this database does not
    // have — a restore to an earlier point leaves exactly this.
    match watermark {
        Some(mark) if checkpoint.through_sequence <= mark => {}
        Some(mark) => {
            return Err(format!(
                "checkpoint runs through {} but the log ends at {}",
                checkpoint.through_sequence.value(),
                mark.value()
            ));
        }
        None => return Err("checkpoint exists but the log is empty".to_owned()),
    }
    if checkpoint.records_ref.media_type != RECORDS_MEDIA_TYPE {
        return Err(format!(
            "records are {:?}, not {RECORDS_MEDIA_TYPE}",
            checkpoint.records_ref.media_type
        ));
    }

    let address = BlobAddress::parse(&checkpoint.records_ref.digest)
        .map_err(|e| format!("records_ref: {e}"))?;
    // The invariant snapshot writes down: the records blob's plaintext IS the
    // bytes that were hashed, so its content address and the projection hash
    // are one digest. Checked before the read, because if they disagree the
    // read would be fetching some other snapshot's records.
    if address.digest_hex() != checkpoint.projection_hash {
        return Err(format!(
            "records address {} is not the projection hash {}",
            address.digest_hex(),
            checkpoint.projection_hash
        ));
    }

    let records = read_blob(blobs, &address, checkpoint.records_ref.byte_size.value())
        .await
        .map_err(|e| format!("read records: {e}"))?;
    if projection_hash(&records) != checkpoint.projection_hash {
        return Err("records do not hash to the recorded projection hash".to_owned());
    }
    // Bytes that hash correctly can still be from a format this build cannot
    // read. Parsing every line is what makes "valid" mean usable.
    for (line, raw) in records.split(|b| *b == b'\n').enumerate() {
        if raw.is_empty() {
            continue;
        }
        serde_json::from_slice::<ProjectionRecord>(raw)
            .map_err(|e| format!("records line {}: {e}", line + 1))?;
    }
    Ok(())
}

/// Read a whole blob, one chunk at a time.
///
/// Bounded by the size the checkpoint row declares rather than by reading until
/// the store stops yielding: a container that hands back more than its row says
/// is not a longer snapshot, it is a disagreement, and the caller's hash check
/// is entitled to see the declared bytes.
async fn read_blob(
    blobs: &PgBlobStore,
    address: &BlobAddress,
    size: u64,
) -> Result<Vec<u8>, gwk_domain::port::BlobError> {
    let mut out = Vec::new();
    while (out.len() as u64) < size {
        let want = (size - out.len() as u64).min(BLOB_CHUNK_BYTES as u64);
        let chunk = blobs
            .read(
                address,
                ByteCount::new(out.len() as u64),
                ByteCount::new(want),
            )
            .await?;
        if chunk.is_empty() {
            return Err(gwk_domain::port::BlobError::Integrity(format!(
                "records end at {} of a declared {size} bytes",
                out.len()
            )));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// The log's highest assigned sequence, read through the caller's connection.
pub(crate) async fn watermark_of(conn: &mut PgConnection) -> Result<Option<Seq>, Refusal> {
    let text: Option<String> = sqlx::query_scalar("SELECT max(seq)::text FROM gwk.event")
        .fetch_one(conn)
        .await
        .map_err(|e| Refusal::storage(format!("watermark: {e}")))?;
    text.map(|t| from_numeric_text(&t))
        .transpose()
        .map(|opt| opt.map(Seq::new))
        .map_err(|e| Refusal::storage(format!("watermark: {e}")))
}

/// The states whose outcome a restart cannot know, as wire strings.
///
/// The FSM already names them and is not asked twice: a state is uncertain
/// exactly when it has a legal edge to `unknown`. `queued` and `leased` do not
/// — an attempt that never started has no outcome to be uncertain about, it
/// simply has not run — and hard-coding a list here would be a second copy of
/// the FSM, free to drift from the one the transitions are checked against.
///
/// Shared with [`crate::ttl_sweep`], which selects on the same set: the report
/// that NAMES a stuck attempt and the sweep that BURIES it have to be talking
/// about the same rows, and one derivation is how that stays true.
pub(crate) fn uncertain_states() -> Result<Vec<String>, Refusal> {
    AttemptState::STATES
        .iter()
        .filter(|state| AttemptState::can_transition(**state, AttemptState::Unknown))
        .map(wire_str)
        .collect()
}

/// Attempts whose outcome a restart cannot know.
async fn uncertain_attempts(conn: &mut PgConnection) -> Result<Vec<String>, Refusal> {
    let states = uncertain_states()?;
    let rows = sqlx::query("SELECT id FROM gwk.attempt WHERE state = ANY($1) ORDER BY id")
        .bind(&states)
        .fetch_all(conn)
        .await
        .map_err(|e| Refusal::storage(format!("read attempts: {e}")))?;
    rows.iter()
        .map(|row| {
            row.try_get::<String, _>(0)
                .map_err(|e| Refusal::storage(format!("attempt id: {e}")))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncertainty_is_read_off_the_fsm_and_excludes_what_never_started() {
        // The one derivation both readers use: this report, and the TTL
        // sweep's stale arm.
        let uncertain = uncertain_states().expect("wire names");
        assert_eq!(
            uncertain,
            ["starting", "running", "blocked", "canceling"],
            "the uncertain set is whatever the FSM says can end in `unknown`"
        );
        // The two that matter for the honest-reporting argument: an attempt
        // that never started has no outcome to be uncertain about.
        assert!(!AttemptState::can_transition(
            AttemptState::Queued,
            AttemptState::Unknown
        ));
        assert!(!AttemptState::can_transition(
            AttemptState::Leased,
            AttemptState::Unknown
        ));
    }

    #[test]
    fn only_a_proven_divergence_blocks_readiness() {
        let report = |verdict| RecoveryReport {
            watermark: None,
            live_hash: projection_hash(&[]),
            verdict,
            rejected: Vec::new(),
            uncertain: Vec::new(),
        };
        assert!(
            report(Verdict::Verified {
                anchor: Seq::new(1)
            })
            .ready()
        );
        assert!(report(Verdict::Replayed { events: 3 }).ready());
        // Unverified is a statement about this run, not an accusation: a crash
        // leaves the newest checkpoint behind the watermark on a kernel whose
        // projections are perfectly correct.
        assert!(
            report(Verdict::Unverified {
                reason: "no valid checkpoint".to_owned()
            })
            .ready()
        );
        assert!(
            !report(Verdict::Diverged {
                expected: "a".repeat(64),
                found: "b".repeat(64),
            })
            .ready()
        );
    }
}
