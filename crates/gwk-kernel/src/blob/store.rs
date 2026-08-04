//! The [`BlobStore`]: containers on a filesystem, keys and bookkeeping in
//! PostgreSQL.
//!
//! The split is the design. Ciphertext is large, immutable, and content-named,
//! which is what a filesystem is good at; the wrapped DEK, the pins, and the
//! tombstone are small, mutable, and have to change atomically, which is what a
//! database is good at. Putting the key in the row rather than the file is what
//! makes rewrap and crypto-shred single writes (ADR 0003, and the container
//! module's own docs).
//!
//! Nothing here takes a path from a caller. A committed blob lives at a path
//! derived from its validated 64-hex digest, and an upload lives at a path
//! derived from an id this store minted — [`is_upload_id`] re-checks that shape
//! on the way back in, because a [`BlobUploadId`] is a plain string newtype that
//! a caller can build out of anything.
//!
//! Write ordering is chosen so every crash window leaves the SAFE remainder:
//!
//! * commit writes the container, then the row. A crash between them leaves a
//!   file whose key exists nowhere — inert ciphertext, and a retried commit
//!   overwrites it.
//! * shred writes the tombstone and drops the key, then unlinks. A crash
//!   between them leaves an unreadable blob, never a readable one.
//! * sweep deletes the row, then unlinks, for the same reason.

use std::path::PathBuf;

use gwk_domain::blob::{BLOB_CHUNK_BYTES, BlobAddress, BlobDescriptor};
use gwk_domain::ids::{BlobUploadId, ByteCount, EvidenceId, Timestamp};
use gwk_domain::port::{BlobError, BlobStore};
use secrecy::ExposeSecret;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use zeroize::Zeroize;

use crate::blob::container::{
    self, CHUNK_LEN_BYTES, DEK_BYTES, MAX_CIPHERTEXT_CHUNK_BYTES, WRAP_NONCE_BYTES,
    WRAPPED_DEK_BYTES,
};
use crate::config::BlobConfig;

/// How long an uncommitted upload survives. Enforced on the way in as well as
/// by [`PgBlobStore::expire_uploads`], so an expired upload is dead even on a
/// deployment where nothing has swept yet.
pub const UPLOAD_EXPIRY_SECS: i64 = 3600;

/// Hex width of a minted upload id.
const UPLOAD_ID_HEX_LEN: usize = 32;

/// The one column list a descriptor is built from. Selected `FROM
/// gwk_internal.blob b`, which the pin sub-select refers to.
///
/// A macro rather than a `const` so call sites can `concat!` it into a real
/// string LITERAL: sqlx 0.9 refuses a runtime-built query unless it is wrapped
/// in `AssertSqlSafe`, and an assertion is a promise where `concat!` is a proof.
macro_rules! blob_columns {
    () => {
        "b.digest, b.media_type, b.byte_size, b.kek_id, b.wrap_nonce, b.wrapped_dek, \
         to_json(b.created_at) #>> '{}' AS created_at, \
         b.tombstoned_at IS NOT NULL AS tombstoned, \
         EXISTS (SELECT 1 FROM gwk_internal.blob_pin p WHERE p.digest = b.digest) AS pinned"
    };
}

/// Blobs no event references and no evidence pins.
///
/// The reference test is a lookup against `event_payload_ref_digest`, the
/// expression index migration 0003 creates — the same extraction, spelled the
/// same way, or PostgreSQL plans a sequential scan of the whole log per
/// candidate instead of an index probe.
///
/// Tombstoned rows are excluded: their ciphertext is already gone, and the row
/// is the only remaining evidence that the blob existed and was destroyed
/// rather than never written. A retention audit needs that difference.
///
/// Checkpoints are the SECOND holder of a reference, and not an optional one.
/// A checkpoint's records blob is committed before the transaction that records
/// the checkpoint row, so a rolled-back append leaves a records blob nothing
/// points at — reclaiming that is the whole reason this runs. Reclaiming a
/// blob a LIVE checkpoint still names is a recovery that cannot run.
macro_rules! unreferenced {
    () => {
        "SELECT b.digest FROM gwk_internal.blob b \
         WHERE b.tombstoned_at IS NULL \
           AND NOT EXISTS (SELECT 1 FROM gwk_internal.blob_pin p WHERE p.digest = b.digest) \
           AND NOT EXISTS (SELECT 1 FROM gwk.event e \
                           WHERE e.payload_ref ->> 'digest' = 'sha256:' || b.digest) \
           AND NOT EXISTS (SELECT 1 FROM gwk_internal.checkpoint c \
                           WHERE c.records_ref ->> 'digest' = 'sha256:' || b.digest)"
    };
}

fn storage(context: &str, error: impl std::fmt::Display) -> BlobError {
    BlobError::Storage(format!("{context}: {error}"))
}

fn integrity(reason: impl Into<String>) -> BlobError {
    BlobError::Integrity(reason.into())
}

/// A frame read that ran off the end of a container is TRUNCATION, not a disk
/// fault. The header declares the plaintext size under authentication, so how
/// many chunks must follow it is not a guess — a file holding fewer has been
/// cut, and that is an integrity failure the caller can act on rather than an
/// opaque storage error it can only retry.
fn frame_error(context: &str, error: std::io::Error) -> BlobError {
    if error.kind() == std::io::ErrorKind::UnexpectedEof {
        return integrity(format!(
            "{context}: the container ends before the chunk its header declares"
        ));
    }
    storage(context, error)
}

/// Is this the shape this store mints? Lowercase hex of a fixed width — no
/// separator, no `.`, no `..`, so it cannot name anything but a leaf under the
/// directory it is joined to.
pub fn is_upload_id(value: &str) -> bool {
    value.len() == UPLOAD_ID_HEX_LEN
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// What a rotation did.
///
/// Two counts rather than one, because a resumed rotation reporting
/// `rewrapped: 0` has SUCCEEDED — every blob was already on the new key — and a
/// single number cannot tell that from a rotation that found nothing to do
/// because it was pointed at a label no blob carries. An operator finishing an
/// interrupted rotation is asking exactly that question.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Rotation {
    /// Blobs this run moved from the old key to the new one.
    pub rewrapped: usize,
    /// Blobs an earlier interrupted run had already moved.
    pub already: usize,
}

/// A committed blob's row, in the form every operation needs it.
struct BlobRow {
    descriptor: BlobDescriptor,
    wrap_nonce: Option<Vec<u8>>,
    wrapped_dek: Option<Vec<u8>>,
}

/// Content-addressed blobs: containers under a root directory, key material and
/// bookkeeping in `gwk_internal`.
pub struct PgBlobStore {
    pool: PgPool,
    config: BlobConfig,
}

impl PgBlobStore {
    /// Bind to an initialized database and prepare the root directory.
    ///
    /// Creating the directories here rather than lazily means a root that is
    /// unwritable is a startup failure, not a failure of the first upload that
    /// happens to arrive in production.
    pub async fn open(pool: PgPool, config: BlobConfig) -> Result<Self, BlobError> {
        let store = Self { pool, config };
        for dir in [store.blob_dir(), store.upload_dir()] {
            fs::create_dir_all(&dir)
                .await
                .map_err(|e| storage(&format!("create {}", dir.display()), e))?;
        }
        store.expire_uploads().await?;
        Ok(store)
    }

    pub fn config(&self) -> &BlobConfig {
        &self.config
    }

    fn blob_dir(&self) -> PathBuf {
        self.config.root().join("blobs")
    }

    fn upload_dir(&self) -> PathBuf {
        self.config.root().join("uploads")
    }

    /// Where a committed container lives: two levels of two hex characters,
    /// then the digest. Sharded because one directory holding every blob in a
    /// deployment is a directory nothing can list.
    fn container_path(&self, digest: &str) -> PathBuf {
        self.blob_dir()
            .join(&digest[0..2])
            .join(&digest[2..4])
            .join(digest)
    }

    fn upload_path(&self, upload_id: &str) -> PathBuf {
        self.upload_dir().join(upload_id)
    }

    /// The staged container a commit builds before it is renamed into place.
    fn staging_path(&self, upload_id: &str) -> PathBuf {
        self.upload_dir().join(format!("{upload_id}.container"))
    }

    /// Validate an id the caller handed back. A shape this store never mints
    /// cannot name anything it wrote, so the honest answer is `NotFound` — not
    /// a message confirming what the id looked like.
    fn upload_id<'a>(&self, upload: &'a BlobUploadId) -> Result<&'a str, BlobError> {
        let id = upload.as_str();
        if !is_upload_id(id) {
            return Err(BlobError::NotFound);
        }
        Ok(id)
    }

    /// Drop uploads that outlived [`UPLOAD_EXPIRY_SECS`], with their files.
    ///
    /// Called from `open`, from `begin`, and from `sweep` rather than by a
    /// timer: an expiry only has to hold for a store somebody is using, and a
    /// background task would be a second thing to supervise for no more
    /// guarantee than this.
    pub async fn expire_uploads(&self) -> Result<(), BlobError> {
        let stale: Vec<String> = sqlx::query_scalar(
            "DELETE FROM gwk_internal.blob_upload \
             WHERE started_at < now() - make_interval(secs => $1::double precision) \
             RETURNING upload_id",
        )
        .bind(UPLOAD_EXPIRY_SECS as f64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| storage("expire uploads", e))?;
        for id in stale {
            self.discard_upload_files(&id).await;
        }
        Ok(())
    }

    /// Best-effort removal of an upload's two possible files. Failures are
    /// ignored on purpose: the row is already gone, so the upload is over
    /// either way, and a leftover file is reclaimed by the next expiry that
    /// reuses the id — never by refusing the operation that just succeeded.
    async fn discard_upload_files(&self, upload_id: &str) {
        let _ = fs::remove_file(self.upload_path(upload_id)).await;
        let _ = fs::remove_file(self.staging_path(upload_id)).await;
    }

    /// Read one committed blob's row.
    async fn row(&self, digest: &str) -> Result<Option<BlobRow>, BlobError> {
        let Some(row) = sqlx::query(concat!(
            "SELECT ",
            blob_columns!(),
            " FROM gwk_internal.blob b WHERE b.digest = $1"
        ))
        .bind(digest)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| storage("read blob row", e))?
        else {
            return Ok(None);
        };
        let get = |name: &str| -> Result<String, BlobError> {
            row.try_get(name)
                .map_err(|e| storage(&format!("column {name}"), e))
        };
        let byte_size: i64 = row
            .try_get("byte_size")
            .map_err(|e| storage("column byte_size", e))?;
        Ok(Some(BlobRow {
            descriptor: BlobDescriptor {
                address: BlobAddress::from_digest(&get("digest")?)
                    .map_err(|e| storage("column digest", e))?,
                media_type: get("media_type")?,
                byte_size: ByteCount::new(
                    u64::try_from(byte_size).map_err(|e| storage("column byte_size", e))?,
                ),
                kek_id: get("kek_id")?,
                created_at: Timestamp::new(get("created_at")?),
                pinned: row
                    .try_get("pinned")
                    .map_err(|e| storage("column pinned", e))?,
                tombstoned: row
                    .try_get("tombstoned")
                    .map_err(|e| storage("column tombstoned", e))?,
            },
            wrap_nonce: row
                .try_get("wrap_nonce")
                .map_err(|e| storage("column wrap_nonce", e))?,
            wrapped_dek: row
                .try_get("wrapped_dek")
                .map_err(|e| storage("column wrapped_dek", e))?,
        }))
    }

    /// The row, or the reason there is nothing to read.
    async fn readable(&self, address: &BlobAddress) -> Result<BlobRow, BlobError> {
        let row = self
            .row(address.digest_hex())
            .await?
            .ok_or(BlobError::NotFound)?;
        if row.descriptor.tombstoned {
            return Err(BlobError::Tombstoned);
        }
        Ok(row)
    }

    /// Rewrap every blob this KEK label covers under `new_kek`, touching no
    /// ciphertext.
    ///
    /// The label does NOT change: it is inside each container's authenticated
    /// header, so a rewrap that relabeled would invalidate the very AAD the new
    /// wrap is bound to. Rotation replaces the key behind the name.
    ///
    /// Re-runnable, and that is not a convenience. One row per statement means
    /// an interruption lands between blobs, leaving a prefix on the new key and
    /// the rest on the old — with, by the paragraph above, the same label on
    /// both. Nothing on the row says which key wrapped it, so a resumed run has
    /// to work that out per blob rather than be told.
    pub async fn rewrap_all(&self, new_kek: &[u8; DEK_BYTES]) -> Result<Rotation, BlobError> {
        let digests: Vec<String> = sqlx::query_scalar(
            "SELECT digest FROM gwk_internal.blob \
             WHERE kek_id = $1 AND tombstoned_at IS NULL ORDER BY digest",
        )
        .bind(self.config.kek_id())
        .fetch_all(&self.pool)
        .await
        .map_err(|e| storage("list blobs to rewrap", e))?;

        let mut report = Rotation::default();
        for digest in &digests {
            let address =
                BlobAddress::from_digest(digest).map_err(|e| storage("listed digest", e))?;
            if self.rewrap_one(&address, new_kek).await? {
                report.rewrapped += 1;
            } else {
                report.already += 1;
            }
        }
        Ok(report)
    }

    /// Move ONE blob's wrapped key onto `new_kek`, or report it already there.
    ///
    /// `Ok(false)` means this store's key does not open the row but `new_kek`
    /// does — the state an interrupted rotation leaves behind. The check is a
    /// second unwrap of 32 bytes against a header already in hand, not a read of
    /// the blob, so asking costs nothing next to the alternative: an AEAD
    /// failure on the first already-rotated blob, indistinguishable from
    /// tampering, stranding a half-rotated store with no way forward.
    pub async fn rewrap_one(
        &self,
        address: &BlobAddress,
        new_kek: &[u8; DEK_BYTES],
    ) -> Result<bool, BlobError> {
        let row = self.readable(address).await?;
        let (wrap_nonce, wrapped_dek) = key_material(&row)?;
        let (_, _, header) = self.open_container(&row.descriptor).await?;
        let new_nonce = container::generate::<aead::consts::U24>()?;
        let new_wrapped = match container::rewrap(
            &header,
            self.config.kek().expose_secret(),
            new_kek,
            &wrap_nonce,
            &wrapped_dek,
            &new_nonce.0,
        ) {
            Ok(wrapped) => wrapped,
            Err(refused) => {
                // The old key did not open it: already rotated, or actually
                // broken. Different answers, so ask rather than assume either.
                let Ok(mut dek) =
                    container::unwrap_dek(&wrapped_dek, new_kek, &wrap_nonce, &header)
                else {
                    // Neither key opens it. The ORIGINAL failure is the fact to
                    // report — the caller asked to rotate off THIS store's key,
                    // and the new key's failure is only how the benign
                    // explanation was ruled out.
                    return Err(refused);
                };
                dek.zeroize();
                return Ok(false);
            }
        };
        // One row, one statement, so an interruption is always between blobs.
        sqlx::query(
            "UPDATE gwk_internal.blob SET wrap_nonce = $2, wrapped_dek = $3 \
             WHERE digest = $1 AND tombstoned_at IS NULL",
        )
        .bind(address.digest_hex())
        .bind(new_nonce.0.as_slice())
        .bind(new_wrapped.as_slice())
        .execute(&self.pool)
        .await
        .map_err(|e| storage("store rewrapped key", e))?;
        Ok(true)
    }

    /// Open a container and read its header — everything the AAD covers, and
    /// nothing else.
    ///
    /// The descriptor supplies the two variable-length fields, so this reads
    /// the header's EXACT width. Reading a maximal header instead would cost
    /// 128 KiB per call, which a ranged read makes once per range: reassembling
    /// a large blob would spend more on re-reading the same 90 bytes than on
    /// the blob.
    async fn open_container(
        &self,
        descriptor: &BlobDescriptor,
    ) -> Result<(fs::File, container::Header, Vec<u8>), BlobError> {
        let path = self.container_path(descriptor.address.digest_hex());
        let mut file = fs::File::open(&path)
            .await
            .map_err(|e| storage(&format!("open {}", path.display()), e))?;
        let expected = container::header_len(&descriptor.media_type, &descriptor.kek_id);
        let mut head = vec![0u8; expected];
        file.read_exact(&mut head)
            .await
            .map_err(|e| frame_error("read container header", e))?;
        // `decode` re-derives the length from the FILE's own `u16` fields. If it
        // lands anywhere but where the row predicted, the AAD would be the wrong
        // bytes — so this is a refusal, not a reason to read again with a
        // corrected length.
        let (header, actual) = container::Header::decode(&head)?;
        if actual != expected {
            return Err(integrity(format!(
                "container header is {actual} bytes, its row describes {expected}"
            )));
        }
        Ok((file, header, head))
    }
}

/// The key material of a blob that has not been shredded.
fn key_material(
    row: &BlobRow,
) -> Result<([u8; WRAP_NONCE_BYTES], [u8; WRAPPED_DEK_BYTES]), BlobError> {
    // The CHECK constraint ties these to the tombstone, so reaching here with
    // either missing means the row was edited around the constraint.
    let (Some(nonce), Some(wrapped)) = (row.wrap_nonce.as_ref(), row.wrapped_dek.as_ref()) else {
        return Err(BlobError::Tombstoned);
    };
    let nonce: [u8; WRAP_NONCE_BYTES] = nonce
        .as_slice()
        .try_into()
        .map_err(|_| integrity("stored wrap nonce is the wrong length"))?;
    let wrapped: [u8; WRAPPED_DEK_BYTES] = wrapped
        .as_slice()
        .try_into()
        .map_err(|_| integrity("stored wrapped key is the wrong length"))?;
    Ok((nonce, wrapped))
}

impl BlobStore for PgBlobStore {
    async fn begin(
        &self,
        media_type: String,
        byte_size: ByteCount,
    ) -> Result<BlobUploadId, BlobError> {
        if media_type.is_empty() || media_type.len() > u16::MAX as usize {
            return Err(integrity(format!(
                "media type must be 1..={} bytes",
                u16::MAX
            )));
        }
        let declared = i64::try_from(byte_size.value())
            .map_err(|_| integrity("declared size does not fit a signed 64-bit column"))?;
        // Reclaim before minting. An abandoned upload holds a staged file, so
        // the expiry has to be driven by something that actually runs, and the
        // path that creates new ones is the one guaranteed to.
        self.expire_uploads().await?;

        let id = container::hex_lower(&container::generate::<aead::consts::U16>()?.0);
        // The row first: if creating the file fails, there is no orphan row to
        // find, because the insert has not committed anything the caller can
        // reach without the file.
        sqlx::query(
            "INSERT INTO gwk_internal.blob_upload (upload_id, media_type, byte_size) \
             VALUES ($1, $2, $3)",
        )
        .bind(&id)
        .bind(&media_type)
        .bind(declared)
        .execute(&self.pool)
        .await
        .map_err(|e| storage("begin upload", e))?;
        fs::File::create(self.upload_path(&id))
            .await
            .map_err(|e| storage("create staging file", e))?;
        Ok(BlobUploadId::new(id))
    }

    async fn write_chunk(
        &self,
        upload: &BlobUploadId,
        sequence: u32,
        chunk: &[u8],
    ) -> Result<(), BlobError> {
        let id = self.upload_id(upload)?;
        let row = sqlx::query(
            "SELECT byte_size, written, next_chunk, \
                    started_at < now() - make_interval(secs => $2::double precision) AS expired \
             FROM gwk_internal.blob_upload WHERE upload_id = $1",
        )
        .bind(id)
        .bind(UPLOAD_EXPIRY_SECS as f64)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| storage("read upload", e))?
        .ok_or(BlobError::NotFound)?;
        let expired: bool = row
            .try_get("expired")
            .map_err(|e| storage("column expired", e))?;
        if expired {
            return Err(BlobError::NotFound);
        }
        let declared: i64 = row
            .try_get("byte_size")
            .map_err(|e| storage("column byte_size", e))?;
        let written: i64 = row
            .try_get("written")
            .map_err(|e| storage("column written", e))?;
        let next_chunk: i64 = row
            .try_get("next_chunk")
            .map_err(|e| storage("column next_chunk", e))?;

        if i64::from(sequence) != next_chunk {
            return Err(integrity(format!(
                "chunk {sequence} is out of order: expected {next_chunk}"
            )));
        }
        let len = i64::try_from(chunk.len())
            .map_err(|_| integrity("chunk does not fit a signed 64-bit count"))?;
        let after = written
            .checked_add(len)
            .ok_or_else(|| integrity("upload size overflowed"))?;
        // The declared size is a budget, not a hint. Without this an upload can
        // fill the disk regardless of what it said it would write.
        if after > declared {
            return Err(integrity(format!(
                "chunk {sequence} would bring the upload to {after} bytes, past the {declared} \
                 it declared"
            )));
        }

        // Written AT its offset rather than appended, so a chunk replayed after
        // a crash between the write and the row update lands on its own bytes
        // instead of after them.
        let path = self.upload_path(id);
        let mut file = fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .await
            .map_err(|e| storage(&format!("open {}", path.display()), e))?;
        file.seek(std::io::SeekFrom::Start(written as u64))
            .await
            .map_err(|e| storage("seek staging file", e))?;
        file.write_all(chunk)
            .await
            .map_err(|e| storage("write chunk", e))?;
        file.flush().await.map_err(|e| storage("flush chunk", e))?;

        sqlx::query(
            "UPDATE gwk_internal.blob_upload SET written = $2, next_chunk = next_chunk + 1 \
             WHERE upload_id = $1",
        )
        .bind(id)
        .bind(after)
        .execute(&self.pool)
        .await
        .map_err(|e| storage("record chunk", e))?;
        Ok(())
    }

    async fn commit(
        &self,
        upload: BlobUploadId,
        address: BlobAddress,
    ) -> Result<(BlobDescriptor, bool), BlobError> {
        let id = self.upload_id(&upload)?.to_owned();
        let row = sqlx::query(
            "SELECT media_type, byte_size, written, \
                    started_at < now() - make_interval(secs => $2::double precision) AS expired \
             FROM gwk_internal.blob_upload WHERE upload_id = $1",
        )
        .bind(&id)
        .bind(UPLOAD_EXPIRY_SECS as f64)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| storage("read upload", e))?
        .ok_or(BlobError::NotFound)?;
        if row
            .try_get::<bool, _>("expired")
            .map_err(|e| storage("column expired", e))?
        {
            return Err(BlobError::NotFound);
        }
        let media_type: String = row
            .try_get("media_type")
            .map_err(|e| storage("column media_type", e))?;
        let declared: i64 = row
            .try_get("byte_size")
            .map_err(|e| storage("column byte_size", e))?;
        let written: i64 = row
            .try_get("written")
            .map_err(|e| storage("column written", e))?;
        if written != declared {
            return Err(integrity(format!(
                "upload holds {written} of the {declared} bytes it declared"
            )));
        }

        // ponytail: the whole plaintext in memory. The container's header binds
        // the digest and the header is the AAD, so NOTHING can be sealed until
        // the last chunk has arrived — a container is a two-pass write however
        // this is written. If blobs ever outgrow a process's memory, make the
        // second pass stream the staged file chunk-by-chunk into the container
        // instead of reading it whole; the format already supports it.
        //
        // Exactly `written` bytes, not the file's length: a chunk replayed at a
        // shorter length can leave stale bytes past the offset the row records,
        // and the row is what the caller's own accounting agreed to.
        let mut plaintext = vec![0u8; written as usize];
        let staged = self.upload_path(&id);
        fs::File::open(&staged)
            .await
            .map_err(|e| storage(&format!("open {}", staged.display()), e))?
            .read_exact(&mut plaintext)
            .await
            .map_err(|e| storage("read staged plaintext", e))?;

        let raw: [u8; 32] = Sha256::digest(&plaintext).into();
        let digest = container::hex_lower(&raw);
        if digest != address.digest_hex() {
            return Err(BlobError::DigestMismatch {
                expected: address,
                actual: BlobAddress::from_digest(&digest)
                    .map_err(|e| storage("computed digest", e))?,
            });
        }

        if let Some(existing) = self.row(&digest).await? {
            self.discard_upload_files(&id).await;
            sqlx::query("DELETE FROM gwk_internal.blob_upload WHERE upload_id = $1")
                .bind(&id)
                .execute(&self.pool)
                .await
                .map_err(|e| storage("close upload", e))?;
            // A shredded address stays shredded. Crypto-shred is a retention
            // decision about an address, and re-presenting the bytes is not an
            // appeal of it — a store that resurrected here would let anyone who
            // kept a copy undo a deletion the audit log calls final.
            if existing.descriptor.tombstoned {
                return Err(BlobError::Tombstoned);
            }
            // Dedup requires digest, size AND media type to match (ADR 0003).
            // Size follows from the digest, so media type is the only one that
            // can disagree — and it cannot mean "a second blob", because the
            // address is the digest alone and both would answer to it. It means
            // the caller and the store disagree about what these bytes are.
            if existing.descriptor.media_type != media_type {
                return Err(integrity(format!(
                    "{address} is already stored as {:?}; this upload declares {media_type:?}",
                    existing.descriptor.media_type
                )));
            }
            return Ok((existing.descriptor, true));
        }

        let mut sealed = container::seal(
            &plaintext,
            &media_type,
            self.config.kek().expose_secret(),
            self.config.kek_id(),
        )?;
        plaintext.zeroize();

        // Staged then renamed: a reader never sees a partially written
        // container, because the name only appears once the bytes are all
        // there. The staging file is in the same directory tree as its
        // destination, so the rename is within one filesystem and atomic.
        let staging = self.staging_path(&id);
        fs::write(&staging, &sealed.container)
            .await
            .map_err(|e| storage("write container", e))?;
        let path = self.container_path(&digest);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| storage("create shard directory", e))?;
        }
        fs::rename(&staging, &path)
            .await
            .map_err(|e| storage("publish container", e))?;

        // The row last. A crash before it leaves a container whose key exists
        // nowhere — unreadable by construction, and overwritten by the retry.
        let inserted: Option<String> = sqlx::query_scalar(
            "INSERT INTO gwk_internal.blob \
               (digest, media_type, byte_size, kek_id, wrap_nonce, wrapped_dek) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (digest) DO NOTHING \
             RETURNING digest",
        )
        .bind(&digest)
        .bind(&media_type)
        .bind(declared)
        .bind(self.config.kek_id())
        .bind(sealed.wrap_nonce.as_slice())
        .bind(sealed.wrapped_dek.as_slice())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| storage("record blob", e))?;
        sealed.wrapped_dek.zeroize();

        sqlx::query("DELETE FROM gwk_internal.blob_upload WHERE upload_id = $1")
            .bind(&id)
            .execute(&self.pool)
            .await
            .map_err(|e| storage("close upload", e))?;
        self.discard_upload_files(&id).await;

        let row = self.row(&digest).await?.ok_or(BlobError::NotFound)?;
        // `None` means a concurrent commit of the identical blob won the
        // insert. Its container is at the same path and holds the same
        // plaintext, so that is a dedup hit that happened to race, not a
        // failure.
        Ok((row.descriptor, inserted.is_none()))
    }

    async fn abort(&self, upload: BlobUploadId) -> Result<(), BlobError> {
        let id = self.upload_id(&upload)?;
        let deleted = sqlx::query("DELETE FROM gwk_internal.blob_upload WHERE upload_id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| storage("abort upload", e))?;
        self.discard_upload_files(id).await;
        if deleted.rows_affected() == 0 {
            return Err(BlobError::NotFound);
        }
        Ok(())
    }

    async fn read(
        &self,
        address: &BlobAddress,
        offset: ByteCount,
        length: ByteCount,
    ) -> Result<Vec<u8>, BlobError> {
        let row = self.readable(address).await?;
        let (wrap_nonce, wrapped_dek) = key_material(&row)?;
        let size = row.descriptor.byte_size.value();
        let offset = offset.value();
        if offset >= size {
            return Ok(Vec::new());
        }
        // Clamped rather than refused, matching the event store's read bound: a
        // huge length is a request for "as much as you'll give me". The ceiling
        // is one chunk, which is also what the wire ships in, so this never
        // buffers more than two chunks to answer.
        let length = length
            .value()
            .min(BLOB_CHUNK_BYTES as u64)
            .min(size - offset);
        if length == 0 {
            return Ok(Vec::new());
        }

        let (mut file, header, aad) = self.open_container(&row.descriptor).await?;
        let header_len = aad.len();
        let aad = aad.as_slice();
        // The row says how big the blob is and the header says so too, under
        // authentication. They are written together and can only disagree if
        // one was edited, and the range arithmetic below trusts both.
        if header.byte_size != size {
            return Err(integrity(format!(
                "container declares {} bytes, its row says {size}",
                header.byte_size
            )));
        }
        let mut dek = container::unwrap_dek(
            &wrapped_dek,
            self.config.kek().expose_secret(),
            &wrap_nonce,
            aad,
        )?;

        let chunk = BLOB_CHUNK_BYTES as u64;
        let final_index = container::chunk_count(size) - 1;
        let first = offset / chunk;
        let last = (offset + length - 1) / chunk;
        let mut plaintext = Vec::with_capacity((length + chunk) as usize);
        let mut framed = vec![0u8; CHUNK_LEN_BYTES + MAX_CIPHERTEXT_CHUNK_BYTES];
        for index in first..=last {
            file.seek(std::io::SeekFrom::Start(container::chunk_offset(
                header_len, index,
            )))
            .await
            .map_err(|e| storage("seek chunk", e))?;
            file.read_exact(&mut framed[..CHUNK_LEN_BYTES])
                .await
                .map_err(|e| frame_error("read chunk length", e))?;
            let ciphertext_len = u32::from_be_bytes(
                framed[..CHUNK_LEN_BYTES]
                    .try_into()
                    .map_err(|_| integrity("chunk length"))?,
            ) as usize;
            // The framing is the one unauthenticated part of a container, so a
            // length is checked against the format's own ceiling before it is
            // used to size a read.
            if ciphertext_len > MAX_CIPHERTEXT_CHUNK_BYTES {
                dek.zeroize();
                return Err(integrity(format!(
                    "chunk {index} declares {ciphertext_len} ciphertext bytes, over the \
                     {MAX_CIPHERTEXT_CHUNK_BYTES} a chunk can hold"
                )));
            }
            let body = &mut framed[CHUNK_LEN_BYTES..CHUNK_LEN_BYTES + ciphertext_len];
            if let Err(e) = file.read_exact(body).await {
                dek.zeroize();
                return Err(frame_error("read chunk", e));
            }
            // `last` comes from the DECLARED size, not from where the file
            // ends — the flag is authenticated, so deriving it from the file
            // would let a truncation define its own final chunk.
            let opened = container::open_chunk(
                aad,
                &dek,
                &header.stream_nonce,
                u32::try_from(index).map_err(|_| integrity("chunk index out of range"))?,
                index == final_index,
                body,
            );
            match opened {
                Ok(part) => plaintext.extend_from_slice(&part),
                Err(e) => {
                    dek.zeroize();
                    return Err(e);
                }
            }
        }
        dek.zeroize();

        let start = (offset - first * chunk) as usize;
        let stop = start + length as usize;
        if plaintext.len() < stop {
            return Err(integrity(format!(
                "container yielded {} bytes where the requested range needs {stop}",
                plaintext.len()
            )));
        }
        Ok(plaintext[start..stop].to_vec())
    }

    async fn stat(&self, address: &BlobAddress) -> Result<Option<BlobDescriptor>, BlobError> {
        match self.row(address.digest_hex()).await? {
            None => Ok(None),
            // Not `Ok(None)`: "never existed" and "existed and was destroyed"
            // are different answers, and a retention audit is asking for the
            // second one.
            Some(row) if row.descriptor.tombstoned => Err(BlobError::Tombstoned),
            Some(row) => Ok(Some(row.descriptor)),
        }
    }

    async fn pin(&self, address: &BlobAddress, evidence: &EvidenceId) -> Result<(), BlobError> {
        self.readable(address).await?;
        sqlx::query(
            "INSERT INTO gwk_internal.blob_pin (digest, evidence_id) VALUES ($1, $2) \
             ON CONFLICT (digest, evidence_id) DO NOTHING",
        )
        .bind(address.digest_hex())
        .bind(evidence.as_str())
        .execute(&self.pool)
        .await
        .map_err(|e| storage("pin blob", e))?;
        Ok(())
    }

    async fn unpin(&self, address: &BlobAddress, evidence: &EvidenceId) -> Result<(), BlobError> {
        // Deliberately NOT `readable`: a shredded blob's pins still have to be
        // releasable, or a retention hold outlives the data it was holding.
        if self.row(address.digest_hex()).await?.is_none() {
            return Err(BlobError::NotFound);
        }
        // Releasing a pin that is not held is the state the caller asked for.
        sqlx::query("DELETE FROM gwk_internal.blob_pin WHERE digest = $1 AND evidence_id = $2")
            .bind(address.digest_hex())
            .bind(evidence.as_str())
            .execute(&self.pool)
            .await
            .map_err(|e| storage("unpin blob", e))?;
        Ok(())
    }

    async fn sweep(&self) -> Result<Vec<BlobAddress>, BlobError> {
        self.expire_uploads().await?;
        // The row goes first and the file second, so an interruption leaves a
        // file nothing can decrypt rather than a row pointing at nothing.
        let swept: Vec<String> = sqlx::query_scalar(concat!(
            "DELETE FROM gwk_internal.blob WHERE digest IN (",
            unreferenced!(),
            ") RETURNING digest"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(|e| storage("sweep blobs", e))?;

        let mut removed = Vec::with_capacity(swept.len());
        for digest in swept {
            let _ = fs::remove_file(self.container_path(&digest)).await;
            removed
                .push(BlobAddress::from_digest(&digest).map_err(|e| storage("swept digest", e))?);
        }
        Ok(removed)
    }

    async fn shred(&self, address: &BlobAddress) -> Result<(), BlobError> {
        let Some(row) = self.row(address.digest_hex()).await? else {
            return Err(BlobError::NotFound);
        };
        // Shred is terminal, so re-running it is the state the caller wants.
        if row.descriptor.tombstoned {
            return Ok(());
        }
        if row.descriptor.pinned {
            return Err(BlobError::Pinned);
        }
        // The tombstone and the key removal are ONE statement, and it commits
        // before any ciphertext is touched (the port's ordering requirement).
        // A crash after this point leaves a blob that answers `Tombstoned`
        // while its container is still on disk — unreadable, which is the whole
        // guarantee. The reverse order would leave a window where the key is
        // still live and the audit trail already says it is not.
        let updated = sqlx::query(
            "UPDATE gwk_internal.blob \
             SET wrap_nonce = NULL, wrapped_dek = NULL, tombstoned_at = now() \
             WHERE digest = $1 AND tombstoned_at IS NULL \
               AND NOT EXISTS (SELECT 1 FROM gwk_internal.blob_pin p WHERE p.digest = $1)",
        )
        .bind(address.digest_hex())
        .execute(&self.pool)
        .await
        .map_err(|e| storage("shred blob", e))?;
        if updated.rows_affected() == 0 {
            // Something pinned it between the read and the write.
            return Err(BlobError::Pinned);
        }
        let _ = fs::remove_file(self.container_path(address.digest_hex())).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_minted_upload_id_can_name_a_file() {
        assert!(is_upload_id(&"a".repeat(UPLOAD_ID_HEX_LEN)));
        assert!(is_upload_id("0123456789abcdef0123456789abcdef"));
        for bad in [
            "",
            "..",
            "../../etc/passwd",
            &"a".repeat(UPLOAD_ID_HEX_LEN - 1),
            &"a".repeat(UPLOAD_ID_HEX_LEN + 1),
            &"A".repeat(UPLOAD_ID_HEX_LEN),
            &"g".repeat(UPLOAD_ID_HEX_LEN),
            "0123456789abcdef0123456789abcde/",
            "0123456789abcdef0123456789abcd.f",
        ] {
            assert!(!is_upload_id(bad), "accepted {bad:?}");
        }
    }
}
