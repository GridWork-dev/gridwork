//! Projection snapshots: the recovery shortcut, never the truth.
//!
//! A checkpoint is the projection tables, canonicalized, hashed, and stored as
//! one encrypted blob at one `global_sequence`. It is EVIDENCE, not a restore
//! point: this schema's `born_initial` and `no_truncate` guards mean projection
//! rows can only ever be produced by replaying the log, so what a checkpoint
//! buys is the ability to check that result, not to skip it (see
//! [`crate::recover`]). The worst a missing or corrupt one costs is a
//! comparison — which is why an append is allowed to take one and none of the
//! contract depends on it existing.
//!
//! Two things make the hash mean anything.
//!
//! **Records are re-serialized through the CONTRACT type, never passed
//! through.** `to_jsonb` returns keys in whatever order the row's physical
//! layout gives, and that layout is a property of one database's history — add
//! a column and it moves. Deserializing each row into its [`ProjectionRecord`]
//! and serializing that back means the bytes are determined by the type
//! declaration instead, so two kernels serving the same log agree, and so does
//! the same kernel after a `VACUUM FULL`.
//!
//! **The visit order is a written-down constant, not a catalog query.** The
//! hash depends on which table comes first, so that has to be something a
//! reader can see and a diff can show changing.
//!
//! The records blob's plaintext IS the canonical bytes, so `projection_hash`
//! and the blob's own content address are the same digest. That is stated as
//! an invariant and asserted, not left as a coincidence for someone to
//! discover while debugging a restore.

use gwk_domain::blob::BlobAddress;
use gwk_domain::checkpoint::{CHECKPOINT_SCHEMA_VERSION, Checkpoint};
use gwk_domain::envelope::PayloadRef;
use gwk_domain::ids::{ByteCount, Seq, Timestamp};
use gwk_domain::port::BlobStore;
use gwk_domain::protocol::{ProjectionKind, ProjectionRecord};
use sha2::{Digest, Sha256};
use sqlx::{PgConnection, Row};

use crate::blob::container;
use crate::blob::store::PgBlobStore;
use crate::numeric::{from_numeric_text, to_numeric_text};
use crate::project::Refusal;

/// What the records blob is: one canonical record per line.
///
/// Line-delimited rather than one JSON array, because recovery streams it back
/// a chunk at a time and a line is a frame it can complete without holding the
/// whole document.
pub const RECORDS_MEDIA_TYPE: &str = "application/x-ndjson";

/// Every projection table, in the order a snapshot visits them, each shaped
/// into the exact wire form of a [`ProjectionRecord`].
///
/// Alphabetical, and spelled out one query at a time. Two kinds of adjustment
/// appear, and both are deliberate rather than convenient:
///
/// * **A `::text` cast on four columns.** `to_jsonb` renders a `numeric` as a
///   JSON NUMBER, while the contract carries 64-bit counters as decimal
///   STRINGS. The difference is invisible until the value passes 2^53, at which
///   point the number silently comes back as a different one — so the cast goes
///   on every such column, not on the ones that have gotten large so far.
///
/// * **Two renamed columns and one subtracted one.** `gwk.receipt` stores
///   `from`/`to` as `from_state`/`to_state` because the bare words are SQL
///   reserved words, and `gwk.orchestrator_checkpoint.updated_at` is row
///   bookkeeping the contract type does not carry — a reader orders those by
///   the checkpoint's own `seq`. Each is handled BY NAME rather than by a
///   general tolerance, so a column added to any other table still fails the
///   round trip. That failure IS the parity check between the DDL and
///   `gwk-domain`, and nothing else in this kernel performs it.
///
/// The `derived` flag is the third adjustment, and the one with teeth. Two
/// tables hold rows the log cannot reproduce:
///
/// * `gwk.receipt` — `submit` writes the authority receipt itself, and on the
///   PAGED path it does so for a command that is REFUSED and therefore appends
///   no event at all ("the one refusal in the kernel that leaves rows behind").
/// * `gwk.attention_item` — the same paged path raises its item directly
///   through `page_attention`, again with no event behind it.
///
/// A hash over those tables could never be reproduced by a replay, so a
/// checkpoint carrying them would fail every scratch rebuild forever and the
/// failure would say nothing. They stay in the canonical dump — that is where
/// the DDL-to-contract parity check happens, and it must cover every table —
/// and stay OUT of the digest. What guards them instead is what always did:
/// `receipt_append_only` and the delete guards, which no privilege can bypass.
struct Projection {
    tag: &'static str,
    /// The column `read` orders and pages by, which is also the field a client
    /// reads a continuation cursor out of. Written down rather than inferred:
    /// fourteen projections key on `id` and `orchestrator_checkpoint` does not,
    /// and a rule that guessed would hand back the wrong cursor for the one
    /// table that differs — a page that silently restarts, not one that fails.
    key: &'static str,
    query: &'static str,
    /// The same record, one page at a time: `$1` a cursor (exclusive), `$2` an
    /// exact key, `$3` the row ceiling. One query serves both reads because
    /// get-by-id IS a one-row page — a second near-identical string per table
    /// would be one more place for the record shape to drift from `query`.
    ///
    /// Ordering and the cursor comparison are both `COLLATE "C"`. Under a
    /// locale collation — which is what the stock PostgreSQL image gives you —
    /// ids differing only in punctuation can TIE, and two ties either side of a
    /// page boundary drop a row or repeat one. Byte order has no ties.
    read: &'static str,
    /// Whether replaying the log through `apply_event` rebuilds this table.
    derived: bool,
}

const fn derived(
    tag: &'static str,
    key: &'static str,
    query: &'static str,
    read: &'static str,
) -> Projection {
    Projection {
        tag,
        key,
        query,
        read,
        derived: true,
    }
}

const fn written_beside_the_log(
    tag: &'static str,
    key: &'static str,
    query: &'static str,
    read: &'static str,
) -> Projection {
    Projection {
        tag,
        key,
        query,
        read,
        derived: false,
    }
}

/// The paged read for one projection and the column it pages by, or `None` if
/// the kind has no table — which cannot happen while
/// [`every_projection_is_visited_exactly_once_in_a_written_down_order`] passes,
/// and is returned rather than panicked because this is reached from a client
/// request.
///
/// The two travel together on purpose: a caller that had the query but chose
/// the cursor field for itself would be free to choose a different one.
pub fn read_query(kind: ProjectionKind) -> Option<(&'static str, &'static str)> {
    PROJECTIONS
        .iter()
        .find(|p| p.tag == kind.as_str())
        .map(|p| (p.read, p.key))
}

const PROJECTIONS: &[Projection] = &[
    derived(
        "attempt",
        "id",
        "SELECT jsonb_build_object('projection_type', 'attempt', 'attempt', to_jsonb(t))::text \
         FROM gwk.attempt t ORDER BY t.id",
        "SELECT jsonb_build_object('projection_type', 'attempt', 'attempt', to_jsonb(t))::text \
         FROM gwk.attempt t \
         WHERE ($1::text IS NULL OR t.id COLLATE \"C\" > $1) \
           AND ($2::text IS NULL OR t.id = $2) \
         ORDER BY t.id COLLATE \"C\" LIMIT $3",
    ),
    written_beside_the_log(
        "attention_item",
        "id",
        "SELECT jsonb_build_object('projection_type', 'attention_item', 'attention_item', \
           to_jsonb(t))::text FROM gwk.attention_item t ORDER BY t.id",
        "SELECT jsonb_build_object('projection_type', 'attention_item', 'attention_item', \
           to_jsonb(t))::text FROM gwk.attention_item t \
         WHERE ($1::text IS NULL OR t.id COLLATE \"C\" > $1) \
           AND ($2::text IS NULL OR t.id = $2) \
         ORDER BY t.id COLLATE \"C\" LIMIT $3",
    ),
    derived(
        "authority_grant",
        "id",
        "SELECT jsonb_build_object('projection_type', 'authority_grant', 'authority_grant', \
           to_jsonb(t))::text FROM gwk.authority_grant t ORDER BY t.id",
        "SELECT jsonb_build_object('projection_type', 'authority_grant', 'authority_grant', \
           to_jsonb(t))::text FROM gwk.authority_grant t \
         WHERE ($1::text IS NULL OR t.id COLLATE \"C\" > $1) \
           AND ($2::text IS NULL OR t.id = $2) \
         ORDER BY t.id COLLATE \"C\" LIMIT $3",
    ),
    derived(
        "command",
        "id",
        "SELECT jsonb_build_object('projection_type', 'command', 'command', to_jsonb(t))::text \
         FROM gwk.command t ORDER BY t.id",
        "SELECT jsonb_build_object('projection_type', 'command', 'command', to_jsonb(t))::text \
         FROM gwk.command t \
         WHERE ($1::text IS NULL OR t.id COLLATE \"C\" > $1) \
           AND ($2::text IS NULL OR t.id = $2) \
         ORDER BY t.id COLLATE \"C\" LIMIT $3",
    ),
    derived(
        "cost_entry",
        "id",
        "SELECT jsonb_build_object('projection_type', 'cost_entry', 'cost_entry', \
           to_jsonb(t) || jsonb_build_object( \
             'input_tokens', t.input_tokens::text, \
             'cached_input_tokens', t.cached_input_tokens::text, \
             'cache_write_tokens', t.cache_write_tokens::text, \
             'output_tokens', t.output_tokens::text, \
             'reasoning_tokens', t.reasoning_tokens::text, \
             'cost_micros', t.cost_micros::text))::text \
         FROM gwk.cost_entry t ORDER BY t.id",
        "SELECT jsonb_build_object('projection_type', 'cost_entry', 'cost_entry', \
           to_jsonb(t) || jsonb_build_object( \
             'input_tokens', t.input_tokens::text, \
             'cached_input_tokens', t.cached_input_tokens::text, \
             'cache_write_tokens', t.cache_write_tokens::text, \
             'output_tokens', t.output_tokens::text, \
             'reasoning_tokens', t.reasoning_tokens::text, \
             'cost_micros', t.cost_micros::text))::text \
         FROM gwk.cost_entry t \
         WHERE ($1::text IS NULL OR t.id COLLATE \"C\" > $1) \
           AND ($2::text IS NULL OR t.id = $2) \
         ORDER BY t.id COLLATE \"C\" LIMIT $3",
    ),
    derived(
        "dispatch_node",
        "id",
        "SELECT jsonb_build_object('projection_type', 'dispatch_node', 'dispatch_node', \
           to_jsonb(t))::text FROM gwk.dispatch_node t ORDER BY t.id",
        "SELECT jsonb_build_object('projection_type', 'dispatch_node', 'dispatch_node', \
           to_jsonb(t))::text FROM gwk.dispatch_node t \
         WHERE ($1::text IS NULL OR t.id COLLATE \"C\" > $1) \
           AND ($2::text IS NULL OR t.id = $2) \
         ORDER BY t.id COLLATE \"C\" LIMIT $3",
    ),
    derived(
        "engine_session",
        "id",
        "SELECT jsonb_build_object('projection_type', 'engine_session', 'engine_session', \
           to_jsonb(t))::text FROM gwk.engine_session t ORDER BY t.id",
        "SELECT jsonb_build_object('projection_type', 'engine_session', 'engine_session', \
           to_jsonb(t))::text FROM gwk.engine_session t \
         WHERE ($1::text IS NULL OR t.id COLLATE \"C\" > $1) \
           AND ($2::text IS NULL OR t.id = $2) \
         ORDER BY t.id COLLATE \"C\" LIMIT $3",
    ),
    derived(
        "evidence",
        "id",
        "SELECT jsonb_build_object('projection_type', 'evidence', 'evidence', \
           to_jsonb(t) || jsonb_build_object('byte_size', t.byte_size::text))::text \
         FROM gwk.evidence t ORDER BY t.id",
        "SELECT jsonb_build_object('projection_type', 'evidence', 'evidence', \
           to_jsonb(t) || jsonb_build_object('byte_size', t.byte_size::text))::text \
         FROM gwk.evidence t \
         WHERE ($1::text IS NULL OR t.id COLLATE \"C\" > $1) \
           AND ($2::text IS NULL OR t.id = $2) \
         ORDER BY t.id COLLATE \"C\" LIMIT $3",
    ),
    derived(
        "gate",
        "id",
        "SELECT jsonb_build_object('projection_type', 'gate', 'gate', to_jsonb(t))::text \
         FROM gwk.gate t ORDER BY t.id",
        "SELECT jsonb_build_object('projection_type', 'gate', 'gate', to_jsonb(t))::text \
         FROM gwk.gate t \
         WHERE ($1::text IS NULL OR t.id COLLATE \"C\" > $1) \
           AND ($2::text IS NULL OR t.id = $2) \
         ORDER BY t.id COLLATE \"C\" LIMIT $3",
    ),
    derived(
        "ingested_record",
        "id",
        "SELECT jsonb_build_object('projection_type', 'ingested_record', 'ingested_record', \
           to_jsonb(t) || jsonb_build_object('event_seq', t.event_seq::text))::text \
         FROM gwk.ingested_record t ORDER BY t.id",
        "SELECT jsonb_build_object('projection_type', 'ingested_record', 'ingested_record', \
           to_jsonb(t) || jsonb_build_object('event_seq', t.event_seq::text))::text \
         FROM gwk.ingested_record t \
         WHERE ($1::text IS NULL OR t.id COLLATE \"C\" > $1) \
           AND ($2::text IS NULL OR t.id = $2) \
         ORDER BY t.id COLLATE \"C\" LIMIT $3",
    ),
    derived(
        "lease",
        "id",
        "SELECT jsonb_build_object('projection_type', 'lease', 'lease', \
           to_jsonb(t) || jsonb_build_object('fence_token', t.fence_token::text))::text \
         FROM gwk.lease t ORDER BY t.id",
        "SELECT jsonb_build_object('projection_type', 'lease', 'lease', \
           to_jsonb(t) || jsonb_build_object('fence_token', t.fence_token::text))::text \
         FROM gwk.lease t \
         WHERE ($1::text IS NULL OR t.id COLLATE \"C\" > $1) \
           AND ($2::text IS NULL OR t.id = $2) \
         ORDER BY t.id COLLATE \"C\" LIMIT $3",
    ),
    derived(
        "message",
        "id",
        "SELECT jsonb_build_object('projection_type', 'message', 'message', to_jsonb(t))::text \
         FROM gwk.message t ORDER BY t.id",
        "SELECT jsonb_build_object('projection_type', 'message', 'message', to_jsonb(t))::text \
         FROM gwk.message t \
         WHERE ($1::text IS NULL OR t.id COLLATE \"C\" > $1) \
           AND ($2::text IS NULL OR t.id = $2) \
         ORDER BY t.id COLLATE \"C\" LIMIT $3",
    ),
    derived(
        "orchestrator_checkpoint",
        "orchestrator_id",
        "SELECT jsonb_build_object('projection_type', 'orchestrator_checkpoint', \
           'orchestrator_checkpoint', \
           (to_jsonb(t) - 'updated_at') || jsonb_build_object('seq', t.seq::text))::text \
         FROM gwk.orchestrator_checkpoint t ORDER BY t.orchestrator_id",
        // Keyed on `orchestrator_id`: this is the one projection whose primary
        // key is not called `id`, and paging it by a column it does not have
        // would fail at the database rather than quietly.
        "SELECT jsonb_build_object('projection_type', 'orchestrator_checkpoint', \
           'orchestrator_checkpoint', \
           (to_jsonb(t) - 'updated_at') || jsonb_build_object('seq', t.seq::text))::text \
         FROM gwk.orchestrator_checkpoint t \
         WHERE ($1::text IS NULL OR t.orchestrator_id COLLATE \"C\" > $1) \
           AND ($2::text IS NULL OR t.orchestrator_id = $2) \
         ORDER BY t.orchestrator_id COLLATE \"C\" LIMIT $3",
    ),
    derived(
        "pty_session",
        "id",
        "SELECT jsonb_build_object('projection_type', 'pty_session', 'pty_session', \
           to_jsonb(t))::text FROM gwk.pty_session t ORDER BY t.id",
        "SELECT jsonb_build_object('projection_type', 'pty_session', 'pty_session', \
           to_jsonb(t))::text FROM gwk.pty_session t \
         WHERE ($1::text IS NULL OR t.id COLLATE \"C\" > $1) \
           AND ($2::text IS NULL OR t.id = $2) \
         ORDER BY t.id COLLATE \"C\" LIMIT $3",
    ),
    derived(
        "pty_session_template",
        "name",
        "SELECT jsonb_build_object('projection_type', 'pty_session_template', \
           'pty_session_template', to_jsonb(t))::text \
         FROM gwk.pty_session_template t ORDER BY t.name",
        "SELECT jsonb_build_object('projection_type', 'pty_session_template', \
           'pty_session_template', to_jsonb(t))::text \
         FROM gwk.pty_session_template t \
         WHERE ($1::text IS NULL OR t.name COLLATE \"C\" > $1) \
           AND ($2::text IS NULL OR t.name = $2) \
         ORDER BY t.name COLLATE \"C\" LIMIT $3",
    ),
    written_beside_the_log(
        "receipt",
        "id",
        "SELECT jsonb_build_object('projection_type', 'receipt', 'receipt', \
           (to_jsonb(t) - 'from_state' - 'to_state') \
           || jsonb_strip_nulls(jsonb_build_object('from', t.from_state, 'to', t.to_state)))::text \
         FROM gwk.receipt t ORDER BY t.id",
        "SELECT jsonb_build_object('projection_type', 'receipt', 'receipt', \
           (to_jsonb(t) - 'from_state' - 'to_state') \
           || jsonb_strip_nulls(jsonb_build_object('from', t.from_state, 'to', t.to_state)))::text \
         FROM gwk.receipt t \
         WHERE ($1::text IS NULL OR t.id COLLATE \"C\" > $1) \
           AND ($2::text IS NULL OR t.id = $2) \
         ORDER BY t.id COLLATE \"C\" LIMIT $3",
    ),
    derived(
        "task",
        "id",
        "SELECT jsonb_build_object('projection_type', 'task', 'task', to_jsonb(t))::text \
         FROM gwk.task t ORDER BY t.id",
        "SELECT jsonb_build_object('projection_type', 'task', 'task', to_jsonb(t))::text \
         FROM gwk.task t \
         WHERE ($1::text IS NULL OR t.id COLLATE \"C\" > $1) \
           AND ($2::text IS NULL OR t.id = $2) \
         ORDER BY t.id COLLATE \"C\" LIMIT $3",
    ),
    derived(
        "workflow_run",
        "id",
        "SELECT jsonb_build_object('projection_type', 'workflow_run', 'workflow_run', \
           to_jsonb(t))::text FROM gwk.workflow_run t ORDER BY t.id",
        "SELECT jsonb_build_object('projection_type', 'workflow_run', 'workflow_run', \
           to_jsonb(t))::text FROM gwk.workflow_run t \
         WHERE ($1::text IS NULL OR t.id COLLATE \"C\" > $1) \
           AND ($2::text IS NULL OR t.id = $2) \
         ORDER BY t.id COLLATE \"C\" LIMIT $3",
    ),
    derived(
        "workspace_node",
        "id",
        "SELECT jsonb_build_object('projection_type', 'workspace_node', 'workspace_node', \
           to_jsonb(t))::text FROM gwk.workspace_node t ORDER BY t.id",
        "SELECT jsonb_build_object('projection_type', 'workspace_node', 'workspace_node', \
           to_jsonb(t))::text FROM gwk.workspace_node t \
         WHERE ($1::text IS NULL OR t.id COLLATE \"C\" > $1) \
           AND ($2::text IS NULL OR t.id = $2) \
         ORDER BY t.id COLLATE \"C\" LIMIT $3",
    ),
    derived(
        "worktree",
        "id",
        "SELECT jsonb_build_object('projection_type', 'worktree', 'worktree', to_jsonb(t))::text \
         FROM gwk.worktree t ORDER BY t.id",
        "SELECT jsonb_build_object('projection_type', 'worktree', 'worktree', to_jsonb(t))::text \
         FROM gwk.worktree t \
         WHERE ($1::text IS NULL OR t.id COLLATE \"C\" > $1) \
           AND ($2::text IS NULL OR t.id = $2) \
         ORDER BY t.id COLLATE \"C\" LIMIT $3",
    ),
];

/// The canonical bytes of every projection row: one record per line, each one
/// having made the round trip through its contract type.
///
/// Reading through the caller's connection is deliberate — inside an append
/// transaction it sees that transaction's own uncommitted projections, which is
/// exactly the state the checkpoint claims to describe.
pub async fn canonical_records(conn: &mut PgConnection) -> Result<Vec<u8>, Refusal> {
    records(conn, false).await
}

/// The subset a replay can rebuild — what a checkpoint actually hashes.
///
/// The two tables left out are written beside the log rather than from it (see
/// [`Projection`]), so including them would produce a digest no rebuild could
/// ever match. Everything downstream — the checkpoint, the readiness compare,
/// the scratch rebuild — uses THIS one, and the difference between the two
/// functions is the whole reason the invariant holds.
pub async fn derived_records(conn: &mut PgConnection) -> Result<Vec<u8>, Refusal> {
    records(conn, true).await
}

async fn records(conn: &mut PgConnection, derived_only: bool) -> Result<Vec<u8>, Refusal> {
    let mut out = Vec::new();
    for projection in PROJECTIONS {
        if derived_only && !projection.derived {
            continue;
        }
        // Read through the struct field, which is a `&'static str`: sqlx 0.9
        // accepts a literal-lifetime query and refuses anything else.
        let rows = sqlx::query(projection.query)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| Refusal::storage(format!("read {} projections: {e}", projection.tag)))?;
        for row in &rows {
            let raw: String = row
                .try_get(0)
                .map_err(|e| Refusal::storage(format!("projection row: {e}")))?;
            // `deny_unknown_fields` on every entity makes this the parity check
            // between the DDL and the contract types: a column with no field
            // fails here rather than silently dropping out of the hash.
            let record: ProjectionRecord = serde_json::from_str(&raw).map_err(|e| {
                Refusal::storage(format!(
                    "projection row does not match the contract type: {e}"
                ))
            })?;
            serde_json::to_writer(&mut out, &record)
                .map_err(|e| Refusal::storage(format!("serialize projection record: {e}")))?;
            out.push(b'\n');
        }
    }
    Ok(out)
}

/// The digest the checkpoint records, over exactly the bytes it stores.
pub fn projection_hash(records: &[u8]) -> String {
    let digest: [u8; 32] = Sha256::digest(records).into();
    container::hex_lower(&digest)
}

/// Snapshot the projections as `conn`'s transaction will leave them, store the
/// records, and record the checkpoint.
///
/// The blob is committed through the blob store's OWN connections, so it lands
/// before the caller's transaction does. If that transaction then rolls back,
/// what is left is a blob nothing references — which is precisely what sweep
/// reclaims, and why sweep has to consider checkpoints as well as events.
///
/// The reverse order is what has no recovery: a checkpoint row committed beside
/// a blob whose write was rolled back is a checkpoint that fails validation
/// forever, and the fallback ladder would walk past it on every single startup.
pub async fn snapshot(
    conn: &mut PgConnection,
    blobs: &PgBlobStore,
    through: Seq,
    created_at: &Timestamp,
) -> Result<Checkpoint, Refusal> {
    let records = derived_records(conn).await?;
    let hash = projection_hash(&records);
    let address =
        BlobAddress::from_digest(&hash).map_err(|e| Refusal::storage(format!("hash: {e}")))?;

    let byte_size = ByteCount::new(records.len() as u64);
    let store_blob = async {
        let upload = blobs
            .begin(RECORDS_MEDIA_TYPE.to_owned(), byte_size)
            .await?;
        for (sequence, chunk) in records
            .chunks(gwk_domain::blob::BLOB_CHUNK_BYTES)
            .enumerate()
        {
            let sequence = u32::try_from(sequence).map_err(|_| {
                gwk_domain::port::BlobError::Storage("snapshot has too many chunks".to_owned())
            })?;
            blobs.write_chunk(&upload, sequence, chunk).await?;
        }
        if records.is_empty() {
            // An empty projection set is a real snapshot: a kernel with no work
            // yet still has a state, and it is the empty one.
            blobs.write_chunk(&upload, 0, &[]).await?;
        }
        blobs.commit(upload, address.clone()).await
    };
    let (descriptor, _deduped) = store_blob
        .await
        .map_err(|e| Refusal::storage(format!("store checkpoint records: {e}")))?;
    // The invariant this whole design rests on: the blob's plaintext IS the
    // bytes that were hashed, so its content address and the projection hash
    // are one digest. If these ever diverge, one of them is describing
    // something other than what was stored.
    debug_assert_eq!(descriptor.address, address);

    let checkpoint = Checkpoint {
        schema_version: CHECKPOINT_SCHEMA_VERSION,
        through_sequence: through,
        projection_hash: hash,
        records_ref: PayloadRef {
            digest: address.as_str().to_owned(),
            media_type: RECORDS_MEDIA_TYPE.to_owned(),
            byte_size,
            retention_class: None,
            evidence_pin: None,
        },
        created_at: created_at.clone(),
    };

    // `DO NOTHING` rather than an upsert: two snapshots at one sequence are the
    // same state by definition, so the second is a no-op and never an
    // overwrite of a checkpoint someone may already be restoring from.
    sqlx::query(
        "INSERT INTO gwk_internal.checkpoint \
           (through_seq, schema_version, projection_hash, records_ref, created_at) \
         VALUES ($1::numeric, $2, $3, $4, $5::timestamptz) \
         ON CONFLICT (through_seq) DO NOTHING",
    )
    .bind(to_numeric_text(through.value()))
    .bind(i64::from(checkpoint.schema_version))
    .bind(&checkpoint.projection_hash)
    .bind(
        serde_json::to_value(&checkpoint.records_ref)
            .map_err(|e| Refusal::storage(format!("serialize records_ref: {e}")))?,
    )
    .bind(created_at.as_str())
    .execute(&mut *conn)
    .await
    .map_err(|e| Refusal::storage(format!("record checkpoint: {e}")))?;

    // The barrier's counters move in the same transaction as the row they
    // describe, so a rolled-back append leaves the barrier exactly where it
    // was and the next one is still due.
    sqlx::query(
        "UPDATE gwk_internal.writer SET checkpoint_seq = $1::numeric, checkpoint_at = $2::timestamptz \
         WHERE id = 1",
    )
    .bind(to_numeric_text(through.value()))
    .bind(created_at.as_str())
    .execute(&mut *conn)
    .await
    .map_err(|e| Refusal::storage(format!("advance the checkpoint barrier: {e}")))?;

    Ok(checkpoint)
}

/// Every checkpoint, newest first — the order the recovery ladder walks.
pub async fn checkpoints(conn: &mut PgConnection) -> Result<Vec<Checkpoint>, Refusal> {
    let rows = sqlx::query(
        "SELECT through_seq::text AS through_text, schema_version, projection_hash, records_ref, \
                to_json(created_at) #>> '{}' AS created_at \
         FROM gwk_internal.checkpoint ORDER BY through_seq DESC",
    )
    .fetch_all(conn)
    .await
    .map_err(|e| Refusal::storage(format!("read checkpoints: {e}")))?;

    rows.iter()
        .map(|row| {
            let get = |name: &str| -> Result<String, Refusal> {
                row.try_get(name)
                    .map_err(|e| Refusal::storage(format!("column {name}: {e}")))
            };
            let schema_version: i64 = row
                .try_get("schema_version")
                .map_err(|e| Refusal::storage(format!("column schema_version: {e}")))?;
            let records_ref: serde_json::Value = row
                .try_get("records_ref")
                .map_err(|e| Refusal::storage(format!("column records_ref: {e}")))?;
            Ok(Checkpoint {
                schema_version: u32::try_from(schema_version)
                    .map_err(|e| Refusal::storage(format!("schema_version: {e}")))?,
                through_sequence: Seq::new(
                    from_numeric_text(&get("through_text")?)
                        .map_err(|e| Refusal::storage(format!("column through_seq: {e}")))?,
                ),
                projection_hash: get("projection_hash")?,
                records_ref: serde_json::from_value(records_ref)
                    .map_err(|e| Refusal::storage(format!("column records_ref: {e}")))?,
                created_at: Timestamp::new(get("created_at")?),
            })
        })
        .collect()
}

/// How many checkpoints this database holds, and the newest sequence one
/// anchors at.
///
/// One statement for the pair, because they are only ever wanted together: the
/// caller is about to discard these rows and is asking how much evidence that
/// costs and which restart's comparison it was.
///
/// A connection rather than a pool, so a caller that must not see the table
/// change between counting it and emptying it can do both on one — and so the
/// applier can ask inside its own transaction.
pub async fn census(conn: &mut PgConnection) -> Result<(i64, Option<Seq>), Refusal> {
    let (held, newest): (i64, Option<String>) =
        sqlx::query_as("SELECT count(*), max(through_seq)::text FROM gwk_internal.checkpoint")
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| Refusal::storage(format!("count checkpoints: {e}")))?;
    let newest = newest
        .map(|text| from_numeric_text(&text).map(Seq::new))
        .transpose()
        .map_err(|e| Refusal::storage(format!("column through_seq: {e}")))?;
    Ok((held, newest))
}

/// Delete every checkpoint and put the barrier back where an empty table says
/// it should be, returning how many rows went.
///
/// **Two statements, and the second is why this is a function rather than a
/// `DELETE` spelled at each call site.** The barrier that decides when the NEXT
/// checkpoint is taken does not live in this table — [`record_checkpoint`]
/// writes `gwk_internal.writer.checkpoint_seq` and `checkpoint_at` in the same
/// transaction as the row they describe, and `checkpoint_due` reads those two
/// and nothing else. Deleting the rows without moving them leaves a database
/// with no checkpoint claiming one was taken moments ago at some sequence: the
/// next append is not due, and on a kernel with no traffic nothing is ever due,
/// so "the first append re-anchors" would be false for up to the whole interval
/// and unbounded while idle. Reset to the epoch and to zero, which is what
/// `checkpoint_due` reads as overdue on both of its terms.
///
/// The caller supplies the connection, so both statements land wherever the
/// caller's atomicity is — inside the applier's transaction, where a refused
/// migration must leave the barrier exactly as it found it.
///
/// An ADMIN act by construction: the runtime role holds SELECT and INSERT on
/// this table and no DELETE, which the privilege sweep asserts and, since this
/// change, the daemon re-checks at every boot. A kernel that could erase the
/// evidence its own recovery is graded against would be able to launder a
/// divergence into an `Unverified` start.
pub async fn discard_all(conn: &mut PgConnection) -> Result<u64, Refusal> {
    discard_all_raw(conn)
        .await
        .map_err(|e| Refusal::storage(format!("discard checkpoints: {e}")))
}

/// [`discard_all`] with the driver's own error, for the applier.
///
/// `migrate::apply` reports `KernelError`, which converts from [`sqlx::Error`]
/// and not from [`Refusal`]. Splitting the wrapper off is what lets both
/// callers share one definition of what discarding IS — the alternative was
/// spelling the two statements twice, and the barrier reset is exactly the kind
/// of second statement a second copy forgets.
pub(crate) async fn discard_all_raw(conn: &mut PgConnection) -> Result<u64, sqlx::Error> {
    let discarded = sqlx::query("DELETE FROM gwk_internal.checkpoint")
        .execute(&mut *conn)
        .await?
        .rows_affected();

    sqlx::query(
        "UPDATE gwk_internal.writer \
            SET checkpoint_seq = 0, checkpoint_at = to_timestamp(0) WHERE id = 1",
    )
    .execute(&mut *conn)
    .await?;

    Ok(discarded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_projection_is_visited_exactly_once_in_a_written_down_order() {
        // The hash depends on this order, so the list is asserted rather than
        // trusted: a table appearing twice would double its rows into the
        // digest, and one appearing under the wrong tag would deserialize into
        // the wrong contract type.
        let ordered: Vec<&str> = PROJECTIONS.iter().map(|p| p.tag).collect();
        let mut tags = ordered.clone();
        tags.sort_unstable();
        tags.dedup();
        assert_eq!(tags.len(), PROJECTIONS.len(), "a tag appears twice");
        assert_eq!(ordered, tags, "the visit order must be alphabetical");

        for projection in PROJECTIONS {
            let tag = projection.tag;
            // Every query names its tag TWICE — once as the `projection_type`
            // value, once as the single field, because that is the shape the
            // contract's records have. Reading the tag off the struct and
            // asserting the SQL agrees is what keeps the flag attached to the
            // table it actually describes.
            assert!(
                projection
                    .query
                    .contains(&format!("'projection_type', '{tag}', '{tag}',")),
                "{tag}: the record's one field must be named for its tag"
            );
            assert!(
                projection.query.contains(&format!(" FROM gwk.{tag} t ")),
                "{tag}: the query must read the table it is tagged for"
            );
            // Unordered rows would hash differently on every read, which is a
            // checkpoint that fails its own validation at random.
            assert!(
                projection.query.contains(" ORDER BY "),
                "{tag}: rows must be ordered"
            );
        }
    }

    #[test]
    fn only_the_tables_a_replay_can_rebuild_reach_the_hash() {
        // The exclusions are named here so adding a table cannot quietly join
        // them, and so the reason survives: `submit` writes both of these
        // itself, and on the paged path it does so for a command that appends
        // NO event — a row no replay will ever produce.
        let excluded: Vec<&str> = PROJECTIONS
            .iter()
            .filter(|p| !p.derived)
            .map(|p| p.tag)
            .collect();
        assert_eq!(excluded, ["attention_item", "receipt"]);
        assert_eq!(
            PROJECTIONS.iter().filter(|p| p.derived).count(),
            PROJECTIONS.len() - 2,
            "everything else must be rebuildable from the log"
        );
    }

    #[test]
    fn the_hash_is_over_the_stored_bytes_and_nothing_else() {
        // Whatever else changes, these two must stay the same function, or a
        // checkpoint's own address stops proving what it contains.
        let records = b"{\"projection_type\":\"task\"}\n".to_vec();
        let hash = projection_hash(&records);
        let address = BlobAddress::from_digest(&hash).expect("a legal address");
        assert_eq!(address.digest_hex(), hash);
        assert_eq!(
            hash,
            {
                let digest: [u8; 32] = Sha256::digest(&records).into();
                container::hex_lower(&digest)
            },
            "the hash must be a plain SHA-256 over the bytes"
        );
        // An empty projection set still has a hash — the digest of nothing.
        assert_eq!(
            projection_hash(&[]),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn a_served_row_is_the_same_row_the_hash_canonicalizes() {
        // Each projection now spells its record-building expression twice, once
        // for the dump and once for the paged read. An edit landing on one and
        // not the other would make the row a client is served differ from the
        // row the checkpoint hashed — a disagreement nothing else in the system
        // is positioned to notice. Only the tail after `FROM` may differ.
        for projection in PROJECTIONS {
            let head = |q: &'static str| {
                q.split_once(" FROM ")
                    .expect("every projection query selects FROM a table")
                    .0
                    .to_owned()
            };
            assert_eq!(
                head(projection.query),
                head(projection.read),
                "{} builds a different record for the hash than for a read",
                projection.tag
            );
        }
    }

    #[test]
    fn the_cursor_key_is_the_column_the_page_was_ordered_by() {
        // `key` is what a client's next cursor is read out of and what the SQL
        // compares that cursor against. If they were ever different columns the
        // page would still return rows — the wrong ones, skipping or repeating
        // at every boundary, with nothing failing.
        for projection in PROJECTIONS {
            let key = projection.key;
            assert!(
                projection
                    .read
                    .contains(&format!("ORDER BY t.{key} COLLATE")),
                "{} pages by {key} but does not order by it",
                projection.tag
            );
            assert!(
                projection
                    .read
                    .contains(&format!("t.{key} COLLATE \"C\" > $1")),
                "{} orders by {key} but compares the cursor against another column",
                projection.tag
            );
        }
    }

    #[test]
    fn every_projection_a_client_can_name_has_a_read_behind_it() {
        // `ProjectionKind` is the request's vocabulary and `PROJECTIONS` is the
        // server's; a kind with no entry is a request that parses, is accepted,
        // and can never be answered.
        for kind in ProjectionKind::ALL {
            assert!(
                read_query(*kind).is_some(),
                "{} has no read query",
                kind.as_str()
            );
        }
    }
}
