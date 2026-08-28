//! The [`ContextCasStore`] adapter: classified blobs over the existing
//! evidence/blob primitives.
//!
//! Nothing here is a second storage engine. Each content class fronts the SAME
//! pool, root, and container format through its own [`PgBlobStore`] carrying
//! that class's KEK (R19) — so sealing, dedup, pins, tombstones, and the
//! sweep's bookkeeping are the shipped code paths, and the container's own
//! AEAD and truncation detection arrive here as a regression canary rather
//! than as new behaviour (R17: the container bytes are untouched).
//!
//! What IS new is the classification row in `gwk.context_blob`, and its write
//! ordering is the design:
//!
//! * put CLAIMS the classification first, then writes bytes. The claim is an
//!   `INSERT .. ON CONFLICT DO NOTHING` followed by a read-back, so two racing
//!   writers of the same digest converge on one classification and the loser
//!   is told exactly how the row disagrees. A crash between claim and bytes
//!   leaves a classification for a blob the CAS does not hold — inert, honest
//!   in `describe` (`blob: None`), and completed by any retry.
//! * The reverse order would be a blob whose class nothing recorded: sealed
//!   under a class KEK the metadata cannot name, invisible to the retention
//!   sweep's class arm, and unreadable through a port that checks classes
//!   first. Every window in this file crashes toward the recoverable side.
//!
//! One collision is refused before any claim: bytes that already exist in the
//! CAS sealed under a key domain no content class owns (a checkpoint snapshot
//! or payload blob with identical content). Classifying those would promise a
//! class KEK that cannot open them.

use gwk_domain::blob::{BLOB_CHUNK_BYTES, BlobAddress};
use gwk_domain::context::{
    BlobClasses, ContentClass, ContextBlobRecord, ContextCasError, ContextCasStore, RedactionClass,
    RetentionClass,
};
use gwk_domain::ids::{ByteCount, EvidenceId, Timestamp};
use gwk_domain::port::{BlobError, BlobStore};
use secrecy::ExposeSecret;
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Row};

use crate::blob::container;
use crate::blob::store::PgBlobStore;
use crate::config::{BlobConfig, ContextBlobConfig};

/// One classification row, as read back from `gwk.context_blob`.
struct ClassRow {
    classes: BlobClasses,
    created_at: Timestamp,
}

/// The Context CAS over PostgreSQL and the shared blob root.
pub struct PgContextCasStore {
    pool: PgPool,
    stores: Vec<(ContentClass, PgBlobStore)>,
}

fn storage(context: &str, error: impl std::fmt::Display) -> ContextCasError {
    ContextCasError::Blob(BlobError::Storage(format!("{context}: {error}")))
}

impl PgContextCasStore {
    /// Bind to an initialized database and the SHARED blob root.
    ///
    /// One store per content class, every one over the same root: the classes
    /// are a key-domain boundary, not a filesystem one, and a second directory
    /// tree would be a second thing to back up for a separation the KEKs
    /// already enforce.
    pub async fn open(
        pool: PgPool,
        root: std::path::PathBuf,
        config: &ContextBlobConfig,
    ) -> Result<Self, BlobError> {
        let mut stores = Vec::with_capacity(ContentClass::ALL.len());
        for class in ContentClass::ALL {
            let (kek, kek_id) = config.kek(class);
            let class_config =
                BlobConfig::new(root.clone(), *kek.expose_secret(), kek_id.to_owned())
                    .map_err(|e| BlobError::Storage(format!("context {class} kek config: {e}")))?;
            stores.push((class, PgBlobStore::open(pool.clone(), class_config).await?));
        }
        Ok(Self { pool, stores })
    }

    /// The class's own store. Constructed over `ContentClass::ALL`, so every
    /// class has one.
    fn store(&self, class: ContentClass) -> &PgBlobStore {
        match self.stores.iter().find(|(seen, _)| *seen == class) {
            Some((_, store)) => store,
            None => unreachable!("one store per content class, by construction"),
        }
    }

    /// Any store, for the class-independent row operations (stat, pins): those
    /// read and write bookkeeping the KEK never touches.
    fn any_store(&self) -> &PgBlobStore {
        &self.stores[0].1
    }

    /// The classification row, or `None`.
    async fn class_row(&self, digest: &BlobAddress) -> Result<Option<ClassRow>, ContextCasError> {
        let Some(row) = sqlx::query(
            "SELECT content_class, redaction_class, retention_class, \
                    to_json(created_at) #>> '{}' AS created_at \
             FROM gwk.context_blob WHERE digest = $1",
        )
        .bind(digest.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| storage("read context classification", e))?
        else {
            return Ok(None);
        };
        let token = |name: &str| -> Result<String, ContextCasError> {
            row.try_get(name)
                .map_err(|e| storage(&format!("column {name}"), e))
        };
        // A token the binary cannot parse means the database carries a class
        // this build does not know — a contract mismatch, refused loudly. The
        // DDL CHECK and the token-parity gate make it unreachable on a healthy
        // estate, and "unreachable on a healthy estate" is exactly what gets a
        // named error instead of a panic.
        let parse_refused = |axis: &str, value: &str| {
            storage(
                "context classification",
                format!("{axis} carries unknown token {value:?}"),
            )
        };
        let content = token("content_class")?;
        let redaction = token("redaction_class")?;
        let retention = token("retention_class")?;
        Ok(Some(ClassRow {
            classes: BlobClasses {
                content: ContentClass::parse(&content)
                    .ok_or_else(|| parse_refused("content_class", &content))?,
                redaction: RedactionClass::parse(&redaction)
                    .ok_or_else(|| parse_refused("redaction_class", &redaction))?,
                retention: RetentionClass::parse(&retention)
                    .ok_or_else(|| parse_refused("retention_class", &retention))?,
            },
            created_at: Timestamp::new(token("created_at")?),
        }))
    }

    /// Claim `classes` for `digest`, or report how the standing claim differs.
    async fn claim(
        &self,
        digest: &BlobAddress,
        classes: BlobClasses,
    ) -> Result<ClassRow, ContextCasError> {
        sqlx::query(
            "INSERT INTO gwk.context_blob \
               (digest, content_class, redaction_class, retention_class) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (digest) DO NOTHING",
        )
        .bind(digest.as_str())
        .bind(classes.content.as_str())
        .bind(classes.redaction.as_str())
        .bind(classes.retention.as_str())
        .execute(&self.pool)
        .await
        .map_err(|e| storage("claim context classification", e))?;
        // Read back rather than trusting the insert: on a conflict the row is
        // whoever won, and the comparison against it is the refusal's content.
        let row = self
            .class_row(digest)
            .await?
            .ok_or_else(|| storage("claim context classification", "the claimed row is absent"))?;
        if row.classes != classes {
            return Err(ContextCasError::ClassMismatch {
                digest: digest.clone(),
                stored: row.classes,
                requested: classes,
            });
        }
        Ok(row)
    }
}

impl ContextCasStore for PgContextCasStore {
    async fn put(
        &self,
        classes: BlobClasses,
        media_type: String,
        plaintext: &[u8],
    ) -> Result<(ContextBlobRecord, bool), ContextCasError> {
        let raw: [u8; 32] = Sha256::digest(plaintext).into();
        let address = BlobAddress::from_digest(&container::hex_lower(&raw))
            .map_err(|e| storage("computed digest", e))?;

        // The foreign-domain check comes BEFORE the claim, so the ordinary
        // collision (bytes already in the CAS as a kernel-internal blob) is
        // refused without writing anything. A blob that arrives between this
        // read and the commit below is caught by the same label comparison
        // after commit; that race can strand a classification row for bytes
        // sealed elsewhere — inert (see the module docs), and the price of
        // never sealing bytes whose class nothing recorded.
        let class_label = self.store(classes.content).config().kek_id().to_owned();
        if self.class_row(&address).await?.is_none()
            && let Some(existing) = self.any_store().descriptor(&address).await?
            && existing.kek_id != class_label
        {
            return Err(ContextCasError::ForeignKeyDomain {
                digest: address,
                stored_kek_id: existing.kek_id,
            });
        }

        let claimed = self.claim(&address, classes).await?;

        let store = self.store(classes.content);
        let upload = store
            .begin(media_type, ByteCount::new(plaintext.len() as u64))
            .await?;
        let mut chunks = plaintext.chunks(BLOB_CHUNK_BYTES);
        let mut sequence = 0u32;
        loop {
            let chunk = chunks.next().unwrap_or(&[]);
            store.write_chunk(&upload, sequence, chunk).await?;
            sequence = sequence.saturating_add(1);
            if chunk.len() < BLOB_CHUNK_BYTES {
                break;
            }
        }
        let (descriptor, deduped) = store.commit(upload, address.clone()).await?;

        // The post-commit half of the foreign-domain race above: a dedup hit
        // is only a hit if the standing container is this CLASS's.
        if descriptor.kek_id != class_label {
            return Err(ContextCasError::ForeignKeyDomain {
                digest: address,
                stored_kek_id: descriptor.kek_id,
            });
        }

        Ok((
            ContextBlobRecord {
                digest: address,
                classes,
                created_at: claimed.created_at,
                blob: Some(descriptor),
            },
            deduped,
        ))
    }

    async fn get(
        &self,
        digest: &BlobAddress,
        content: ContentClass,
    ) -> Result<Vec<u8>, ContextCasError> {
        let row = self
            .class_row(digest)
            .await?
            .ok_or(ContextCasError::NotFound)?;
        if row.classes.content != content {
            return Err(ContextCasError::WrongContentClass {
                digest: digest.clone(),
                stored: row.classes.content,
                requested: content,
            });
        }
        let store = self.store(content);
        let descriptor = store.stat(digest).await?.ok_or(ContextCasError::NotFound)?;
        let size = descriptor.byte_size.value();
        let mut out = Vec::with_capacity(size as usize);
        while (out.len() as u64) < size {
            let part = store
                .read(
                    digest,
                    ByteCount::new(out.len() as u64),
                    ByteCount::new(size - out.len() as u64),
                )
                .await?;
            if part.is_empty() {
                return Err(storage(
                    "read context blob",
                    "the read stalled short of its size",
                ));
            }
            out.extend_from_slice(&part);
        }
        Ok(out)
    }

    async fn describe(
        &self,
        digest: &BlobAddress,
    ) -> Result<Option<ContextBlobRecord>, ContextCasError> {
        let Some(row) = self.class_row(digest).await? else {
            return Ok(None);
        };
        Ok(Some(ContextBlobRecord {
            digest: digest.clone(),
            classes: row.classes,
            created_at: row.created_at,
            blob: self.any_store().descriptor(digest).await?,
        }))
    }

    async fn pin(
        &self,
        digest: &BlobAddress,
        evidence: &EvidenceId,
    ) -> Result<(), ContextCasError> {
        // Context-scoped: this port pins classified blobs, and answering for
        // an unclassified digest would make it a second door to the general
        // pin surface.
        if self.class_row(digest).await?.is_none() {
            return Err(ContextCasError::NotFound);
        }
        Ok(self.any_store().pin(digest, evidence).await?)
    }

    async fn unpin(
        &self,
        digest: &BlobAddress,
        evidence: &EvidenceId,
    ) -> Result<(), ContextCasError> {
        if self.class_row(digest).await?.is_none() {
            return Err(ContextCasError::NotFound);
        }
        Ok(self.any_store().unpin(digest, evidence).await?)
    }
}
