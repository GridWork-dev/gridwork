//! Projection snapshots: the recovery shortcut, never the truth.
//!
//! A checkpoint is the projection tables, canonicalized, hashed, and stored as
//! one encrypted blob at one `global_sequence`. Recovery ALWAYS replays the
//! suffix after it, so the worst a missing or corrupt checkpoint costs is time
//! — which is why an append is allowed to take one and none of the contract
//! depends on it existing.
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
use gwk_domain::protocol::ProjectionRecord;
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
/// * **A `::text` cast on three columns.** `to_jsonb` renders a `numeric` as a
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
const PROJECTIONS: &[&str] = &[
    "SELECT jsonb_build_object('projection_type', 'attempt', 'attempt', to_jsonb(t))::text \
     FROM gwk.attempt t ORDER BY t.id",
    "SELECT jsonb_build_object('projection_type', 'attention_item', 'attention_item', \
       to_jsonb(t))::text FROM gwk.attention_item t ORDER BY t.id",
    "SELECT jsonb_build_object('projection_type', 'authority_grant', 'authority_grant', \
       to_jsonb(t))::text FROM gwk.authority_grant t ORDER BY t.id",
    "SELECT jsonb_build_object('projection_type', 'command', 'command', to_jsonb(t))::text \
     FROM gwk.command t ORDER BY t.id",
    "SELECT jsonb_build_object('projection_type', 'dispatch_node', 'dispatch_node', \
       to_jsonb(t))::text FROM gwk.dispatch_node t ORDER BY t.id",
    "SELECT jsonb_build_object('projection_type', 'engine_session', 'engine_session', \
       to_jsonb(t))::text FROM gwk.engine_session t ORDER BY t.id",
    "SELECT jsonb_build_object('projection_type', 'evidence', 'evidence', \
       to_jsonb(t) || jsonb_build_object('byte_size', t.byte_size::text))::text \
     FROM gwk.evidence t ORDER BY t.id",
    "SELECT jsonb_build_object('projection_type', 'gate', 'gate', to_jsonb(t))::text \
     FROM gwk.gate t ORDER BY t.id",
    "SELECT jsonb_build_object('projection_type', 'lease', 'lease', \
       to_jsonb(t) || jsonb_build_object('fence_token', t.fence_token::text))::text \
     FROM gwk.lease t ORDER BY t.id",
    "SELECT jsonb_build_object('projection_type', 'message', 'message', to_jsonb(t))::text \
     FROM gwk.message t ORDER BY t.id",
    "SELECT jsonb_build_object('projection_type', 'orchestrator_checkpoint', \
       'orchestrator_checkpoint', \
       (to_jsonb(t) - 'updated_at') || jsonb_build_object('seq', t.seq::text))::text \
     FROM gwk.orchestrator_checkpoint t ORDER BY t.orchestrator_id",
    "SELECT jsonb_build_object('projection_type', 'receipt', 'receipt', \
       (to_jsonb(t) - 'from_state' - 'to_state') \
       || jsonb_strip_nulls(jsonb_build_object('from', t.from_state, 'to', t.to_state)))::text \
     FROM gwk.receipt t ORDER BY t.id",
    "SELECT jsonb_build_object('projection_type', 'task', 'task', to_jsonb(t))::text \
     FROM gwk.task t ORDER BY t.id",
    "SELECT jsonb_build_object('projection_type', 'worktree', 'worktree', to_jsonb(t))::text \
     FROM gwk.worktree t ORDER BY t.id",
];

/// The canonical bytes of every projection row: one record per line, each one
/// having made the round trip through its contract type.
///
/// Reading through the caller's connection is deliberate — inside an append
/// transaction it sees that transaction's own uncommitted projections, which is
/// exactly the state the checkpoint claims to describe.
pub async fn canonical_records(conn: &mut PgConnection) -> Result<Vec<u8>, Refusal> {
    let mut out = Vec::new();
    // Destructured to a `&'static str`: sqlx 0.9 accepts a literal-lifetime
    // query and refuses anything else, so the borrow the loop hands out has to
    // be dereferenced rather than asserted safe.
    for &query in PROJECTIONS {
        let rows = sqlx::query(query)
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| Refusal::storage(format!("read projections: {e}")))?;
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
    let records = canonical_records(conn).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_projection_is_visited_exactly_once_in_a_written_down_order() {
        // The hash depends on this order, so the list is asserted rather than
        // trusted: a table appearing twice would double its rows into the
        // digest, and one appearing under the wrong tag would deserialize into
        // the wrong contract type.
        let mut tags: Vec<&str> = PROJECTIONS
            .iter()
            .map(|q| {
                let after = q.split("'projection_type', '").nth(1).expect("a tag");
                after.split('\'').next().expect("a closing quote")
            })
            .collect();
        let ordered = tags.clone();
        tags.sort_unstable();
        tags.dedup();
        assert_eq!(tags.len(), PROJECTIONS.len(), "a tag appears twice");
        assert_eq!(ordered, tags, "the visit order must be alphabetical");

        // Every query names its tag TWICE — once as the `projection_type`
        // value, once as the single field, because that is the shape the
        // contract's records have.
        for (query, tag) in PROJECTIONS.iter().zip(&ordered) {
            assert!(
                query.contains(&format!("'projection_type', '{tag}', '{tag}',")),
                "{tag}: the record's one field must be named for its tag"
            );
            // Unordered rows would hash differently on every read, which is a
            // checkpoint that fails its own validation at random.
            assert!(query.contains(" ORDER BY "), "{tag}: rows must be ordered");
        }
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
}
