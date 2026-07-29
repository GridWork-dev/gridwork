//! The PostgreSQL [`EventStore`].
//!
//! Ordering is the whole job. `global_sequence` is assigned inside the append
//! transaction, under the `gwk_internal.writer` row lock, and that lock is held
//! until commit — so the order values are handed out IS the order transactions
//! commit in. Unique and strictly increasing, deliberately not gapless: a
//! rolled-back append gives its number back, a crashed one may not, and no
//! reader is allowed to care.
//!
//! This is why the column is not `BIGSERIAL`. A sequence allocates at INSERT
//! time while transactions commit in another order, so a reader paging an
//! allocation-ordered column can see N+1 committed before N exists and then
//! never see N at all. That hole cannot be patched by reading more carefully;
//! it has to be closed at allocation.
//!
//! Writes are admitted through a bounded permit set and then serialize on that
//! same row lock. The bound is what turns a flood into a refusal instead of an
//! unbounded queue of connections waiting on one row.

use gwk_domain::checkpoint::{CHECKPOINT_EVENT_INTERVAL, CHECKPOINT_INTERVAL_SECS};
use gwk_domain::envelope::{Actor, EventEnvelope, INLINE_PAYLOAD_MAX_BYTES, Origin, PayloadRef};
use gwk_domain::ids::{
    AggregateId, CorrelationId, EventId, FenceToken, IdempotencyKey, ProjectId, Seq, Timestamp,
};
use gwk_domain::port::{AppendError, EventStore, MAX_READ_LIMIT, StorageError};
use secrecy::{ExposeSecret, SecretString};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor, PgConnection, PgPool, Row, postgres::PgRow};
use tokio::sync::{Semaphore, SemaphorePermit};

// Aliased, not imported bare: this module's signatures are the port's, whose
// error types are AppendError/StorageError, so a `Result<T>` in scope that
// means `Result<T, KernelError>` would shadow exactly the wrong thing.
use crate::blob::store::PgBlobStore;
use crate::checkpoint;
use crate::error::Result as KernelResult;
use crate::numeric::{from_numeric_text, to_numeric_text};
use crate::writer::claim_epoch;

/// How many appends may be in flight before the store refuses.
///
/// They all serialize on one row lock, so this is a queue depth, not a
/// parallelism setting. Its job is to make overload a fast, typed refusal
/// rather than a growing pile of connections blocked on the same row.
pub const MAX_INFLIGHT_APPENDS: usize = 64;

/// The wake-up channel. Carries the new watermark, but only as a hint: a
/// consumer that never receives a single notification still recovers the whole
/// suffix from its durable cursor, which is what makes this channel droppable.
pub const EVENT_CHANNEL: &str = "gwk_event";

/// Reported as "presented" when an append omitted a fence entirely. Real tokens
/// start at 1, so this cannot collide with one.
const NO_FENCE_PRESENTED: u64 = 0;

/// Every column of an event, shaped for [`row_to_envelope`].
///
/// `seq` crosses as decimal text (see [`crate::numeric`]). Timestamps cross
/// through `to_json`, which renders RFC 3339 with microseconds — exact, and
/// stable because [`connect_pool`] pins every session to UTC.
///
/// A macro rather than a `const` so callers can `concat!` it into a real string
/// LITERAL. sqlx 0.9 refuses a runtime-built query unless it is wrapped in
/// `AssertSqlSafe`, and an assertion is a promise; `concat!` is a proof — the
/// compiler can see there is no runtime data in these statements at all.
macro_rules! event_columns {
    () => {
        "seq::text AS seq_text, event_id, project_id, aggregate_type, \
         aggregate_id, aggregate_version, event_type, schema_version, \
         to_json(occurred_at) #>> '{}' AS occurred_at, \
         to_json(appended_at) #>> '{}' AS appended_at, \
         actor, origin, causation_id, correlation_id, idempotency_key, payload, payload_ref"
    };
}

/// Open a pool whose sessions are pinned to UTC.
///
/// Timestamps are read back through `to_json`, which renders in the session's
/// time zone. Without this every deployment would embed its server's local
/// offset in the log's text, and a rebuild on a differently configured host
/// would not be byte-identical.
pub async fn connect_pool(
    database_url: &SecretString,
    max_connections: u32,
) -> KernelResult<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(max_connections)
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                conn.execute("SET TIME ZONE 'UTC'").await?;
                Ok(())
            })
        })
        .connect(database_url.expose_secret())
        .await?;
    Ok(pool)
}

/// The PostgreSQL event store.
pub struct PgEventStore {
    pool: PgPool,
    admission: Semaphore,
    boot_epoch: i64,
    /// Where a checkpoint's records go. `None` means this store takes no
    /// snapshots — see [`PgEventStore::with_blobs`].
    blobs: Option<PgBlobStore>,
}

impl PgEventStore {
    /// Bind to an initialized database and claim this boot's writer epoch.
    ///
    /// Claiming here rather than lazily means a superseded process discovers it
    /// at startup, not at the first append.
    pub async fn open(pool: PgPool) -> KernelResult<Self> {
        Self::with_capacity(pool, MAX_INFLIGHT_APPENDS).await
    }

    /// [`open`](Self::open) with an explicit admission bound.
    ///
    /// Exists so a test can saturate the queue deterministically — proving the
    /// bound refuses rather than queues needs a bound small enough to fill.
    pub async fn with_capacity(pool: PgPool, inflight: usize) -> KernelResult<Self> {
        let boot_epoch = claim_epoch(&pool).await?;
        Ok(Self {
            pool,
            admission: Semaphore::new(inflight),
            boot_epoch,
            blobs: None,
        })
    }

    /// A store that can READ the log and cannot write it.
    ///
    /// It claims no epoch, and that is the whole point. `claim_epoch` BUMPS a
    /// durable counter, so a verification path that opened an ordinary store
    /// would fence the daemon it was verifying out of its own log — the
    /// rebuild-into-scratch comparison is meant to run against a live system,
    /// not to take it down.
    ///
    /// Epoch 0 is never a live epoch, so the fence every mutating transaction
    /// already compares refuses this store's appends by construction. A reader
    /// cannot write even by mistake, and the guarantee costs no new check.
    pub fn open_reader(pool: PgPool) -> Self {
        Self {
            pool,
            admission: Semaphore::new(MAX_INFLIGHT_APPENDS),
            boot_epoch: 0,
            blobs: None,
        }
    }

    /// Attach the blob store a checkpoint writes its records to.
    ///
    /// Without it this store never checkpoints, and that is a real state rather
    /// than a degraded one: `admin init` and the certifier drive the log with no
    /// filesystem at all, and a snapshot they neither want nor could store is
    /// not something to make them configure around. What must not happen is a
    /// SERVING kernel in that state, which is why the daemon's own construction
    /// takes the blob store rather than opting into it.
    #[must_use]
    pub fn with_blobs(mut self, blobs: PgBlobStore) -> Self {
        self.blobs = Some(blobs);
        self
    }

    /// The blob store checkpoints write through, if one is attached.
    pub fn blobs(&self) -> Option<&PgBlobStore> {
        self.blobs.as_ref()
    }

    /// The checkpoint barrier, if either bound has tripped.
    ///
    /// Called AFTER the caller has written this batch's projections, and that
    /// ordering is the whole correctness argument. A checkpoint claims to
    /// describe the state through a sequence; taken inside `append_locked` it
    /// would run before the projections of the very events it names, producing
    /// a snapshot one batch stale that then fails its own validation forever.
    /// The test that caught it compares the stored hash against a re-read.
    ///
    /// Still inside the caller's transaction and still under the writer row
    /// lock, which is what "writer barrier" means: every other appender is
    /// blocked, so the snapshot sees this transaction's rows and nobody's
    /// half-written ones.
    ///
    /// A snapshot that FAILS fails the append. Swallowing it would be worse by
    /// far — a barrier that silently stops firing grows recovery time without
    /// bound, and the first anyone hears of it is a restart that replays the
    /// entire log.
    pub(crate) async fn checkpoint_if_due(
        &self,
        tx: &mut PgConnection,
        writer: &WriterState,
        through: Seq,
        created_at: &Timestamp,
    ) -> Result<(), AppendError> {
        let Some(blobs) = &self.blobs else {
            return Ok(());
        };
        if !writer.checkpoint_due(through.value()) {
            return Ok(());
        }
        checkpoint::snapshot(tx, blobs, through, created_at)
            .await
            .map(|_| ())
            .map_err(|e| AppendError::Storage(format!("checkpoint: {e}")))
    }

    /// The epoch this process must present for the rest of its life.
    pub fn boot_epoch(&self) -> i64 {
        self.boot_epoch
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Take one admission permit, or refuse.
    ///
    /// `try_acquire` rather than `acquire`: under load the right answer is a
    /// refusal the caller can act on, not a queue.
    pub(crate) fn admit(&self) -> Result<SemaphorePermit<'_>, AppendError> {
        self.admission.try_acquire().map_err(|_| {
            AppendError::Storage(format!(
                "append queue is full ({MAX_INFLIGHT_APPENDS} in flight)"
            ))
        })
    }

    /// Lock the writer row for the rest of this transaction and prove this
    /// process still holds write authority.
    ///
    /// ONE locked read: the epoch to compare, the fence to check, the sequence
    /// to allocate. The lock is held to commit, which is what makes allocation
    /// order equal commit order — so a caller that will DECIDE something from a
    /// read (the command path reading an aggregate's current version) must take
    /// this lock BEFORE that read, or its decision races the writer it is about
    /// to become.
    pub(crate) async fn lock_writer(
        &self,
        tx: &mut PgConnection,
    ) -> Result<WriterState, AppendError> {
        let writer = sqlx::query(
            "SELECT epoch, fence_token::text AS fence_text, next_seq::text AS next_seq_text, \
                    checkpoint_seq::text AS checkpoint_seq_text, \
                    now() - checkpoint_at >= make_interval(secs => $1::double precision) \
                      AS checkpoint_overdue \
             FROM gwk_internal.writer WHERE id = 1 FOR UPDATE",
        )
        .bind(CHECKPOINT_INTERVAL_SECS as f64)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| append_storage("lock writer row", e))?
        .ok_or_else(|| {
            AppendError::Storage(
                "gwk_internal.writer has no singleton row: database not initialized".to_owned(),
            )
        })?;

        let epoch: i64 = writer
            .try_get("epoch")
            .map_err(|e| append_storage("column epoch", e))?;
        if epoch != self.boot_epoch {
            // Terminal, not retryable: only a restart can claim a fresh epoch.
            // The writer-lock probe cancels the server on the same event; this
            // is the guard for work already in flight when that happened.
            return Err(AppendError::Storage(format!(
                "writer epoch {} was superseded by {epoch}: another kernel took write authority",
                self.boot_epoch
            )));
        }

        let current_fence: Option<u64> = writer
            .try_get::<Option<String>, _>("fence_text")
            .map_err(|e| append_storage("column fence_token", e))?
            .map(|text| from_numeric_text(&text))
            .transpose()
            .map_err(|e| append_storage("column fence_token", e))?;
        let next_seq = from_numeric_text(
            &writer
                .try_get::<String, _>("next_seq_text")
                .map_err(|e| append_storage("column next_seq", e))?,
        )
        .map_err(|e| append_storage("column next_seq", e))?;

        let checkpoint_seq = from_numeric_text(
            &writer
                .try_get::<String, _>("checkpoint_seq_text")
                .map_err(|e| append_storage("column checkpoint_seq", e))?,
        )
        .map_err(|e| append_storage("column checkpoint_seq", e))?;
        let checkpoint_overdue: bool = writer
            .try_get("checkpoint_overdue")
            .map_err(|e| append_storage("column checkpoint_at", e))?;

        Ok(WriterState {
            next_seq,
            current_fence,
            checkpoint_seq,
            checkpoint_overdue,
        })
    }

    /// Append one aggregate's batch inside a transaction that already holds the
    /// writer row lock, WITHOUT committing.
    ///
    /// Split out so the command path can write its projections into the same
    /// transaction: the plan requires events and projections to land together,
    /// and duplicating this fence/idempotency/CAS/allocate sequence to get that
    /// is how the two would drift.
    pub(crate) async fn append_locked(
        &self,
        tx: &mut PgConnection,
        writer: &WriterState,
        expected_version: u32,
        fence: Option<FenceToken>,
        events: &[EventEnvelope],
    ) -> Result<Appended, AppendError> {
        validate_batch(expected_version, events)?;
        let first = &events[0];
        let aggregate_type = first.aggregate_type.clone();
        let aggregate_id = first.aggregate_id.as_str().to_owned();

        // Before the first grant any token is accepted, including none. After
        // it, the current token is mandatory — omitting one must not be a way
        // around the check.
        if let Some(current) = writer.current_fence {
            let presented = fence.map(|f| f.value()).unwrap_or(NO_FENCE_PRESENTED);
            if presented != current {
                return Err(AppendError::Fenced {
                    presented: FenceToken::new(presented),
                    current: FenceToken::new(current),
                });
            }
        }

        // Idempotency BEFORE the CAS. A retry presents the SAME
        // `expected_version` it did the first time, which the CAS would now
        // reject as a conflict — so a replay has to be recognized before the
        // version check ever runs, or retry-stability is unreachable.
        let keys: Vec<String> = events
            .iter()
            .filter_map(|e| e.idempotency_key.as_ref())
            .map(|k| k.as_str().to_owned())
            .collect();
        if !keys.is_empty() {
            let stored = sqlx::query(concat!(
                "SELECT ",
                event_columns!(),
                " FROM gwk.event \
                 WHERE aggregate_type = $1 AND aggregate_id = $2 AND idempotency_key = ANY($3) \
                 ORDER BY seq"
            ))
            .bind(&aggregate_type)
            .bind(&aggregate_id)
            .bind(&keys)
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| append_storage("idempotency lookup", e))?;

            if !stored.is_empty() {
                if stored.len() == events.len() && keys.len() == events.len() {
                    let stored = stored
                        .iter()
                        .map(row_to_envelope)
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|e| AppendError::Storage(e.0))?;
                    // A count match is NOT a replay. This lookup is scoped to
                    // the aggregate — the scope of the index it guards — so the
                    // rows it found may belong to a different request entirely:
                    // another project's, or the same project's under a reused
                    // key. Answering those as a replay would hand the caller
                    // someone else's events and report a command that never ran
                    // as applied. The batch has to BE the stored one.
                    if let Some((stored, presented)) = stored
                        .iter()
                        .zip(events)
                        .find(|(stored, presented)| !is_replay_of(stored, presented))
                    {
                        return Err(AppendError::MalformedBatch(format!(
                            "idempotency key {:?} already named {}/{} version {} in project {}: \
                             a retry must present the identical batch",
                            presented
                                .idempotency_key
                                .as_ref()
                                .map(|k| k.as_str())
                                .unwrap_or_default(),
                            stored.aggregate_type,
                            stored.aggregate_id,
                            stored.aggregate_version,
                            stored.project_id,
                        )));
                    }
                    // Every event in this batch already landed under its key:
                    // the original result, replayed.
                    return Ok(Appended {
                        events: stored,
                        replayed: true,
                    });
                }
                return Err(AppendError::MalformedBatch(format!(
                    "{} of {} idempotency keys already landed for {aggregate_type}/{aggregate_id}: \
                     a retry must present the identical batch",
                    stored.len(),
                    events.len()
                )));
            }
        }

        // CAS against the aggregate's current version.
        let actual = current_aggregate_version(&mut *tx, &aggregate_type, &aggregate_id)
            .await
            .map_err(|e| AppendError::Storage(e.0))?;
        if actual != expected_version {
            return Err(AppendError::VersionConflict {
                actual,
                expected: expected_version,
            });
        }

        let mut appended = Vec::with_capacity(events.len());
        for (offset, event) in events.iter().enumerate() {
            let seq = writer
                .next_seq
                .checked_add(offset as u64)
                .ok_or_else(|| AppendError::Storage("global sequence exhausted".to_owned()))?;
            // `now()` is transaction time, so every event in one commit shares
            // an `appended_at` — which is the truth: they landed together.
            let row = sqlx::query(concat!(
                "INSERT INTO gwk.event ( \
                   seq, event_id, project_id, aggregate_type, aggregate_id, aggregate_version, \
                   event_type, schema_version, occurred_at, appended_at, actor, origin, \
                   causation_id, correlation_id, idempotency_key, payload, payload_ref) \
                 VALUES ($1::numeric, $2, $3, $4, $5, $6, $7, $8, $9::timestamptz, now(), \
                   $10, $11, $12, $13, $14, $15, $16) \
                 RETURNING ",
                event_columns!()
            ))
            .bind(to_numeric_text(seq))
            .bind(event.event_id.as_str())
            .bind(event.project_id.as_str())
            .bind(&event.aggregate_type)
            .bind(event.aggregate_id.as_str())
            .bind(i64::from(event.aggregate_version))
            .bind(&event.event_type)
            .bind(i64::from(event.schema_version))
            .bind(event.occurred_at.as_str())
            .bind(serde_json::to_value(&event.actor).map_err(|e| append_storage("actor", e))?)
            .bind(serde_json::to_value(&event.origin).map_err(|e| append_storage("origin", e))?)
            .bind(event.causation_id.as_ref().map(|v| v.as_str()))
            .bind(event.correlation_id.as_ref().map(|v| v.as_str()))
            .bind(event.idempotency_key.as_ref().map(|v| v.as_str()))
            .bind(&event.payload)
            .bind(
                event
                    .payload_ref
                    .as_ref()
                    .map(serde_json::to_value)
                    .transpose()
                    .map_err(|e| append_storage("payload_ref", e))?,
            )
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| append_storage("insert event", e))?;
            appended.push(row_to_envelope(&row).map_err(|e| AppendError::Storage(e.0))?);
        }

        let advanced = writer
            .next_seq
            .checked_add(events.len() as u64)
            .ok_or_else(|| AppendError::Storage("global sequence exhausted".to_owned()))?;
        sqlx::query("UPDATE gwk_internal.writer SET next_seq = $1::numeric WHERE id = 1")
            .bind(to_numeric_text(advanced))
            .execute(&mut *tx)
            .await
            .map_err(|e| append_storage("advance sequence", e))?;

        // Queued inside the transaction, delivered by PostgreSQL only if it
        // commits — so a wake-up never announces an append that rolled back.
        sqlx::query("SELECT pg_notify($1, $2)")
            .bind(EVENT_CHANNEL)
            .bind(to_numeric_text(advanced - 1))
            .execute(&mut *tx)
            .await
            .map_err(|e| append_storage("notify", e))?;

        Ok(Appended {
            events: appended,
            replayed: false,
        })
    }
}

/// The writer row as read under its lock: what an append needs to allocate a
/// sequence and prove it is still the writer.
pub(crate) struct WriterState {
    pub(crate) next_seq: u64,
    pub(crate) current_fence: Option<u64>,
    /// The sequence the last snapshot covered through.
    pub(crate) checkpoint_seq: u64,
    /// Whether [`CHECKPOINT_INTERVAL_SECS`] has passed since that snapshot.
    /// Evaluated in the database, under the same lock, so every appender in
    /// every process is reading one clock.
    pub(crate) checkpoint_overdue: bool,
}

impl WriterState {
    /// Is a snapshot due at `through`? Either bound trips it.
    fn checkpoint_due(&self, through: u64) -> bool {
        self.checkpoint_overdue
            || through.saturating_sub(self.checkpoint_seq) >= CHECKPOINT_EVENT_INTERVAL
    }
}

/// What an append produced, and whether it was a keyed retry.
///
/// `replayed` matters to a caller writing side rows in the same transaction:
/// a replay's projections were written by the original append, and re-applying
/// them would advance a CAS version the events do not account for.
pub(crate) struct Appended {
    pub(crate) events: Vec<EventEnvelope>,
    pub(crate) replayed: bool,
}

/// Every event in a project that landed under one idempotency key.
///
/// Project-wide, not per-aggregate: one key names one request, so this is the
/// lookup that tells a retry (same key, same target, same body) from a reuse
/// (same key, anything else).
///
/// It spans BOTH unique indexes, because either one taking the key is enough to
/// make the write impossible: `event_idempotency_project` on
/// `(project_id, idempotency_key)`, and `event_idempotency` on
/// `(aggregate_type, aggregate_id, idempotency_key)`. Neither contains the
/// other — a key used on this same aggregate by a DIFFERENT project is invisible
/// to the first and fatal to the second — so reading only one leaves the caller
/// to discover the collision as a constraint violation instead of a typed
/// refusal.
pub(crate) async fn events_for_key(
    conn: &mut PgConnection,
    project_id: &str,
    aggregate_type: &str,
    aggregate_id: &str,
    key: &str,
) -> Result<Vec<EventEnvelope>, StorageError> {
    let rows = sqlx::query(concat!(
        "SELECT ",
        event_columns!(),
        " FROM gwk.event \
         WHERE idempotency_key = $4 \
           AND (project_id = $1 OR (aggregate_type = $2 AND aggregate_id = $3)) \
         ORDER BY seq"
    ))
    .bind(project_id)
    .bind(aggregate_type)
    .bind(aggregate_id)
    .bind(key)
    .fetch_all(conn)
    .await
    .map_err(|e| storage("idempotency lookup", e))?;
    rows.iter().map(row_to_envelope).collect()
}

/// The highest version an aggregate has reached, `0` when it has no events.
pub(crate) async fn current_aggregate_version(
    conn: &mut PgConnection,
    aggregate_type: &str,
    aggregate_id: &str,
) -> Result<u32, StorageError> {
    let actual: Option<i64> = sqlx::query_scalar(
        "SELECT max(aggregate_version) FROM gwk.event \
         WHERE aggregate_type = $1 AND aggregate_id = $2",
    )
    .bind(aggregate_type)
    .bind(aggregate_id)
    .fetch_one(conn)
    .await
    .map_err(|e| storage("read aggregate version", e))?;
    u32::try_from(actual.unwrap_or(0)).map_err(|e| storage("aggregate_version out of range", e))
}

/// What a batch must be before it reaches the database.
///
/// Checked in Rust rather than left to constraint violations, so a caller gets
/// one clear reason instead of whichever index happened to fire first.
pub fn validate_batch(expected_version: u32, events: &[EventEnvelope]) -> Result<(), AppendError> {
    let malformed = |reason: String| Err(AppendError::MalformedBatch(reason));
    let Some(first) = events.first() else {
        return malformed("empty batch".to_owned());
    };
    for (offset, event) in events.iter().enumerate() {
        if event.aggregate_type != first.aggregate_type || event.aggregate_id != first.aggregate_id
        {
            return malformed(format!(
                "batch mixes aggregates: {}/{} and {}/{}",
                first.aggregate_type,
                first.aggregate_id.as_str(),
                event.aggregate_type,
                event.aggregate_id.as_str()
            ));
        }
        // Contiguous from expected_version + 1. Checked with u64 arithmetic so
        // a batch that would run past u32::MAX is refused rather than wrapping.
        let want = u64::from(expected_version) + 1 + offset as u64;
        if want > u64::from(u32::MAX) {
            return malformed(format!("aggregate_version would exceed {}", u32::MAX));
        }
        if u64::from(event.aggregate_version) != want {
            return malformed(format!(
                "aggregate_version {} at offset {offset}: expected {want} \
                 (contiguous from expected_version + 1)",
                event.aggregate_version
            ));
        }
        // The column holds the envelope's full u32 range (bigint, CHECK-bounded
        // to [1, 2^32-1]) — the only value left to refuse is the one below it.
        // Caught here so a caller gets a named field instead of a CHECK
        // violation, and refused rather than defaulted: the store may assign
        // `global_sequence` and `appended_at` and nothing else, so inventing a
        // schema_version would be the store inventing contract — permanently,
        // in an append-only log, on the one field a decoder dispatches on.
        if event.schema_version == 0 {
            return malformed(format!(
                "schema_version 0 at offset {offset} is not a version"
            ));
        }
        // The log carries bounded metadata; anything larger belongs in a blob.
        let inline = serde_json::to_vec(&event.payload)
            .map(|bytes| bytes.len())
            .unwrap_or(usize::MAX);
        if inline > INLINE_PAYLOAD_MAX_BYTES {
            return malformed(format!(
                "inline payload at offset {offset} is {inline} bytes, over the \
                 {INLINE_PAYLOAD_MAX_BYTES} bound — use payload_ref"
            ));
        }
        // One key may identify at most one event per aggregate, so a batch that
        // repeats one can never land. Caught here rather than left to the
        // unique index, which would surface as an opaque storage error on the
        // first attempt and a confusing partial-replay refusal on the retry.
        if let Some(key) = &event.idempotency_key
            && events[..offset]
                .iter()
                .any(|earlier| earlier.idempotency_key.as_ref() == Some(key))
        {
            return malformed(format!(
                "idempotency key {:?} appears twice in one batch",
                key.as_str()
            ));
        }
    }
    Ok(())
}

fn storage(context: &str, error: impl std::fmt::Display) -> StorageError {
    StorageError(format!("{context}: {error}"))
}

fn append_storage(context: &str, error: impl std::fmt::Display) -> AppendError {
    AppendError::Storage(format!("{context}: {error}"))
}

/// Rebuild an envelope from a row selected with [`EVENT_COLUMNS`].
/// Read one page of the log through any executor.
///
/// Split out of [`EventStore::read_from`], which always reads from the pool,
/// because recovery has to stream the log from inside its OWN snapshot
/// transaction. A replay whose result is compared against a hash must see one
/// fixed log; reading through a second connection would let the watermark move
/// underneath it and turn a live append into a spurious divergence.
pub(crate) async fn read_page<'e, E>(
    executor: E,
    cursor: Option<Seq>,
    limit: usize,
) -> Result<Vec<EventEnvelope>, StorageError>
where
    E: sqlx::PgExecutor<'e>,
{
    // Clamped, not refused: the port documents a huge limit as a request
    // for "as much as you'll give me".
    let limit = limit.min(MAX_READ_LIMIT) as i64;
    // The `IS NULL OR` reads like it would defeat the primary-key index. It
    // does not: the parameter is a constant at plan time, so PostgreSQL
    // folds the branch away and plans an Index Only Scan with
    // `Index Cond: (seq > $1)` — identical to writing the bare comparison.
    // Checked on 16 with EXPLAIN; the null case folds to a plain ordered
    // index scan, which is what "read from the beginning" wants anyway.
    let rows = sqlx::query(concat!(
        "SELECT ",
        event_columns!(),
        " FROM gwk.event \
         WHERE $1::numeric IS NULL OR seq > $1::numeric \
         ORDER BY seq LIMIT $2"
    ))
    .bind(cursor.map(|c| to_numeric_text(c.value())))
    .bind(limit)
    .fetch_all(executor)
    .await
    .map_err(|e| storage("read_from", e))?;
    rows.iter().map(row_to_envelope).collect()
}

fn row_to_envelope(row: &PgRow) -> Result<EventEnvelope, StorageError> {
    let get = |name: &str| -> Result<String, StorageError> {
        row.try_get::<String, _>(name)
            .map_err(|e| storage(&format!("column {name}"), e))
    };
    let opt = |name: &str| -> Result<Option<String>, StorageError> {
        row.try_get::<Option<String>, _>(name)
            .map_err(|e| storage(&format!("column {name}"), e))
    };
    let json = |name: &str| -> Result<serde_json::Value, StorageError> {
        row.try_get::<serde_json::Value, _>(name)
            .map_err(|e| storage(&format!("column {name}"), e))
    };
    let seq_text = get("seq_text")?;
    let seq = from_numeric_text(&seq_text).map_err(|e| storage("column seq", e))?;
    let aggregate_version: i64 = row
        .try_get("aggregate_version")
        .map_err(|e| storage("column aggregate_version", e))?;
    // i64, not i32: the column is bigint so it can hold the envelope's whole u32
    // range. Reading it narrow is not a silent truncation — the driver refuses
    // the decode outright — but it does make every read of a valid log fail.
    let schema_version: i64 = row
        .try_get("schema_version")
        .map_err(|e| storage("column schema_version", e))?;
    let payload_ref: Option<serde_json::Value> = row
        .try_get("payload_ref")
        .map_err(|e| storage("column payload_ref", e))?;

    Ok(EventEnvelope {
        event_id: EventId::new(get("event_id")?),
        project_id: ProjectId::new(get("project_id")?),
        aggregate_type: get("aggregate_type")?,
        aggregate_id: AggregateId::new(get("aggregate_id")?),
        // The column is CHECK-bounded to the u32 range in the contract DDL, so
        // a value outside it means the schema was tampered with, not that the
        // conversion needs a fallback.
        aggregate_version: u32::try_from(aggregate_version)
            .map_err(|e| storage("aggregate_version out of range", e))?,
        event_type: get("event_type")?,
        schema_version: u32::try_from(schema_version)
            .map_err(|e| storage("schema_version out of range", e))?,
        global_sequence: Seq::new(seq),
        occurred_at: Timestamp::new(get("occurred_at")?),
        appended_at: Timestamp::new(get("appended_at")?),
        actor: serde_json::from_value::<Actor>(json("actor")?)
            .map_err(|e| storage("column actor", e))?,
        origin: serde_json::from_value::<Origin>(json("origin")?)
            .map_err(|e| storage("column origin", e))?,
        causation_id: opt("causation_id")?.map(EventId::new),
        correlation_id: opt("correlation_id")?.map(CorrelationId::new),
        idempotency_key: opt("idempotency_key")?.map(IdempotencyKey::new),
        payload: json("payload")?,
        payload_ref: payload_ref
            .map(serde_json::from_value::<PayloadRef>)
            .transpose()
            .map_err(|e| storage("column payload_ref", e))?,
    })
}

/// Is `stored` the event that `presented` is re-presenting?
///
/// Compares the WHOLE envelope with the fields the caller does not control
/// normalized away, rather than a hand-listed subset of the ones it does: a
/// field added to [`EventEnvelope`] later joins this comparison automatically,
/// where a list would quietly stop covering it and start passing a differing
/// request off as a retry.
///
/// Three fields are excluded. `global_sequence` and `appended_at` are assigned
/// by the store. `occurred_at` is normalized by the column: it goes in as
/// whatever RFC 3339 spelling the caller used and comes back in PostgreSQL's,
/// so `Z` becomes `+00:00` and an identical retry would otherwise never match.
fn is_replay_of(stored: &EventEnvelope, presented: &EventEnvelope) -> bool {
    let mut normalized = presented.clone();
    normalized.global_sequence = stored.global_sequence;
    normalized.appended_at = stored.appended_at.clone();
    normalized.occurred_at = stored.occurred_at.clone();
    normalized == *stored
}

impl EventStore for PgEventStore {
    async fn append(
        &self,
        expected_version: u32,
        fence: Option<FenceToken>,
        events: Vec<EventEnvelope>,
    ) -> Result<Vec<EventEnvelope>, AppendError> {
        let _permit = self.admit()?;
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| append_storage("begin", e))?;
        let writer = self.lock_writer(&mut tx).await?;
        let appended = self
            .append_locked(&mut tx, &writer, expected_version, fence, &events)
            .await?;
        // Committed even on a replay: nothing was written, so this only
        // releases the writer lock the replay lookup was read under.
        tx.commit().await.map_err(|e| append_storage("commit", e))?;
        Ok(appended.events)
    }

    async fn read_from(
        &self,
        cursor: Option<Seq>,
        limit: usize,
    ) -> Result<Vec<EventEnvelope>, StorageError> {
        read_page(&self.pool, cursor, limit).await
    }

    async fn watermark(&self) -> Result<Option<Seq>, StorageError> {
        let text: Option<String> = sqlx::query_scalar("SELECT max(seq)::text FROM gwk.event")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| storage("watermark", e))?;
        text.map(|t| from_numeric_text(&t))
            .transpose()
            .map(|opt| opt.map(Seq::new))
            .map_err(|e| storage("watermark", e))
    }

    async fn grant_fence(&self) -> Result<FenceToken, StorageError> {
        let text: String = sqlx::query_scalar(
            "UPDATE gwk_internal.writer \
             SET fence_token = coalesce(fence_token, 0) + 1 \
             WHERE id = 1 RETURNING fence_token::text",
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| storage("grant_fence", e))?
        .ok_or_else(|| StorageError("gwk_internal.writer has no singleton row".to_owned()))?;
        from_numeric_text(&text)
            .map(FenceToken::new)
            .map_err(|e| storage("grant_fence", e))
    }
}

#[cfg(test)]
mod tests {
    use gwk_domain::ids::Timestamp;

    use super::*;

    fn event(aggregate_id: &str, version: u32) -> EventEnvelope {
        EventEnvelope {
            event_id: EventId::new(format!("evt-{aggregate_id}-{version}")),
            project_id: ProjectId::new("p"),
            aggregate_type: "task".into(),
            aggregate_id: AggregateId::new(aggregate_id),
            aggregate_version: version,
            event_type: "tick".into(),
            schema_version: 1,
            global_sequence: Seq::new(0),
            occurred_at: Timestamp::new("2026-01-01T00:00:00Z"),
            appended_at: Timestamp::new("2026-01-01T00:00:00Z"),
            actor: Actor {
                kind: "kernel".into(),
                id: None,
            },
            origin: Origin {
                system: "kernel".into(),
                r#ref: None,
            },
            causation_id: None,
            correlation_id: None,
            idempotency_key: None,
            payload: serde_json::json!({}),
            payload_ref: None,
        }
    }

    #[test]
    fn a_batch_is_one_aggregate_contiguous_from_the_expected_version() {
        validate_batch(0, &[event("a", 1), event("a", 2)]).expect("contiguous from 1");
        validate_batch(7, &[event("a", 8)]).expect("contiguous from expected + 1");

        for (expected, batch, why) in [
            (0u32, vec![], "empty"),
            (0, vec![event("a", 2)], "starts past expected + 1"),
            (0, vec![event("a", 1), event("a", 3)], "gap"),
            (0, vec![event("a", 1), event("a", 1)], "repeat"),
            (5, vec![event("a", 1)], "ignores expected_version"),
        ] {
            let err = validate_batch(expected, &batch).expect_err(why);
            assert!(
                matches!(err, AppendError::MalformedBatch(_)),
                "{why}: {err:?}"
            );
        }

        let mixed = vec![event("a", 1), event("b", 2)];
        let err = validate_batch(0, &mixed).expect_err("mixed aggregates");
        let AppendError::MalformedBatch(reason) = err else {
            panic!("expected MalformedBatch");
        };
        assert!(reason.contains("mixes aggregates"), "{reason}");
    }

    #[test]
    fn a_batch_may_not_run_past_the_aggregate_version_ceiling() {
        let mut last = event("a", u32::MAX);
        last.aggregate_version = u32::MAX;
        validate_batch(u32::MAX - 1, &[last.clone()]).expect("the final version is reachable");
        // One more would need u32::MAX + 1, which must be refused rather than wrap.
        let err = validate_batch(u32::MAX, &[last]).expect_err("past the ceiling");
        let AppendError::MalformedBatch(reason) = err else {
            panic!("expected MalformedBatch");
        };
        assert!(reason.contains("would exceed"), "{reason}");
    }

    #[test]
    fn the_column_carries_every_schema_version_the_envelope_can_declare() {
        // The column was `integer` and silently could not hold the top half of
        // the envelope's u32 (3-prime NB-3). It is bigint now, so the whole
        // declared range must round-trip through validation untouched.
        for version in [1, i32::MAX as u32, i32::MAX as u32 + 1, u32::MAX] {
            let mut e = event("a", 1);
            e.schema_version = version;
            validate_batch(0, &[e]).unwrap_or_else(|err| panic!("schema_version {version}: {err}"));
        }

        let mut zero = event("a", 1);
        zero.schema_version = 0;
        let err = validate_batch(0, &[zero]).expect_err("0 is not a version");
        let AppendError::MalformedBatch(reason) = err else {
            panic!("expected MalformedBatch");
        };
        assert!(reason.contains("schema_version"), "{reason}");
    }

    #[test]
    fn one_key_may_not_appear_twice_in_a_batch() {
        let key = |k: &str| Some(gwk_domain::ids::IdempotencyKey::new(k));
        let mut a = event("a", 1);
        let mut b = event("a", 2);
        a.idempotency_key = key("same");
        b.idempotency_key = key("same");
        let err = validate_batch(0, &[a.clone(), b.clone()]).expect_err("repeated key");
        let AppendError::MalformedBatch(reason) = err else {
            panic!("expected MalformedBatch");
        };
        assert!(reason.contains("twice in one batch"), "{reason}");

        // Distinct keys, and a keyed event beside an unkeyed one, are both fine.
        b.idempotency_key = key("other");
        validate_batch(0, &[a.clone(), b.clone()]).expect("distinct keys");
        b.idempotency_key = None;
        validate_batch(0, &[a, b]).expect("a mix of keyed and unkeyed");
    }

    #[test]
    fn an_oversized_inline_payload_is_refused_before_it_reaches_the_log() {
        let mut big = event("a", 1);
        big.payload = serde_json::json!({ "blob": "x".repeat(INLINE_PAYLOAD_MAX_BYTES) });
        let err = validate_batch(0, &[big]).expect_err("oversized payload");
        let AppendError::MalformedBatch(reason) = err else {
            panic!("expected MalformedBatch");
        };
        assert!(reason.contains("payload_ref"), "{reason}");
    }
}
