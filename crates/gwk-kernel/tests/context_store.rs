//! Certifies the Context CAS adapter: classified put/get, the per-class KEK
//! boundary, closed class sets at the database, retention by class, and the
//! pin override.
//!
//! What only a real database and a real filesystem can show: that the
//! classification claim and the bytes converge under retries and disagreeing
//! writers; that the class boundary is enforced twice (metadata refusal, then
//! AEAD failure under the wrong class key); that a class token outside the
//! contract's closed set dies at INSERT; that the retention sweep reclaims
//! exactly what an expired class window stops protecting and nothing a pin or
//! a missing window still protects; and that the v1 container's own integrity
//! behaviour arrives through the adapter unchanged (R17 — a regression canary,
//! not new behaviour).
//!
//! `#[ignore]` because it needs a server — see `tests/common/mod.rs`.

mod common;

use common::{drop_database, maintenance_pool, raw_store};
use gwk_domain::blob::{BLOB_CHUNK_BYTES, BlobAddress};
use gwk_domain::context::{
    BlobClasses, ContentClass, ContextCasError, ContextCasStore, RedactionClass, RetentionClass,
};
use gwk_domain::ids::{ByteCount, EvidenceId};
use gwk_domain::port::{BlobError, BlobStore};
use gwk_kernel::blob::context::PgContextCasStore;
use gwk_kernel::blob::store::PgBlobStore;
use gwk_kernel::config::{BlobConfig, ContextBlobConfig};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

/// Distinct per-class key material. Constants so the swapped-KEK arm can build
/// the mis-keyed ring exactly.
const CONFORMANCE_KEK: [u8; 32] = [0x21; 32];
const PRIVATE_KEK: [u8; 32] = [0x22; 32];

fn ring() -> ContextBlobConfig {
    ContextBlobConfig::new(vec![
        (
            ContentClass::Conformance,
            CONFORMANCE_KEK,
            "kek-ctx-conformance".to_owned(),
        ),
        (
            ContentClass::Private,
            PRIVATE_KEK,
            "kek-ctx-private".to_owned(),
        ),
    ])
    .expect("a complete ring")
}

/// The same ring with the two keys behind the labels exchanged — the PLAN's
/// swapped-KEK mutation, held as a live arm rather than a one-off edit.
fn swapped_ring() -> ContextBlobConfig {
    ContextBlobConfig::new(vec![
        (
            ContentClass::Conformance,
            PRIVATE_KEK,
            "kek-ctx-conformance".to_owned(),
        ),
        (
            ContentClass::Private,
            CONFORMANCE_KEK,
            "kek-ctx-private".to_owned(),
        ),
    ])
    .expect("a complete ring")
}

fn classes(
    content: ContentClass,
    redaction: RedactionClass,
    retention: RetentionClass,
) -> BlobClasses {
    BlobClasses {
        content,
        redaction,
        retention,
    }
}

fn private_manifest() -> BlobClasses {
    classes(
        ContentClass::Private,
        RedactionClass::Redacted,
        RetentionClass::Manifest,
    )
}

/// Incompressible plaintext, as in `blob_store.rs`: a chunk landing at the
/// wrong offset shows up as wrong bytes rather than the same bytes.
fn bytes(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 37 + i / 89) % 251) as u8).collect()
}

fn address_of(plaintext: &[u8]) -> BlobAddress {
    let digest: [u8; 32] = Sha256::digest(plaintext).into();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    BlobAddress::from_digest(&hex).expect("digest")
}

/// A fresh root shared by the adapter and (where a case needs one) the main
/// kernel store, beside `store`'s database.
fn fresh_root(tag: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!("gwk-ctx-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    root
}

async fn adapter(pool: &PgPool, root: &std::path::Path) -> PgContextCasStore {
    PgContextCasStore::open(pool.clone(), root.to_path_buf(), &ring())
        .await
        .expect("open the context store")
}

/// Seed a classification row directly, with its claim backdated `age_days`.
///
/// The table is append-only under an ENABLE ALWAYS trigger, so even a
/// superuser cannot age a row by UPDATE — which is the guarantee under test
/// elsewhere. Retention cases therefore seed the aged claim FIRST; the
/// adapter's put then converges on it (same classes, same digest), exactly the
/// retried-put path the module docs describe.
async fn seed_aged_claim(pool: &PgPool, digest: &BlobAddress, c: BlobClasses, age_days: i32) {
    sqlx::query(
        "INSERT INTO gwk.context_blob \
           (digest, content_class, redaction_class, retention_class, created_at) \
         VALUES ($1, $2, $3, $4, now() - make_interval(days => $5))",
    )
    .bind(digest.as_str())
    .bind(c.content.as_str())
    .bind(c.redaction.as_str())
    .bind(c.retention.as_str())
    .bind(age_days)
    .execute(pool)
    .await
    .expect("seed an aged classification");
}

async fn teardown(maintenance: &PgPool, name: &str, root: &std::path::Path) {
    drop_database(maintenance, name).await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_classified_blob_round_trips_and_the_classification_is_singular() {
    let maintenance = maintenance_pool().await;
    let (name, store) = raw_store(&maintenance, "ctxround", 8).await;
    let root = fresh_root("ctxround");
    let cas = adapter(store.pool(), &root).await;

    // Three container chunks, the last one short — the shape every boundary
    // case lives in.
    let plaintext = bytes(BLOB_CHUNK_BYTES * 2 + 7);
    let (record, deduped) = cas
        .put(
            private_manifest(),
            "application/json".to_owned(),
            &plaintext,
        )
        .await
        .expect("put");
    assert!(!deduped);
    assert_eq!(record.digest, address_of(&plaintext));
    assert_eq!(record.classes, private_manifest());
    let descriptor = record.blob.expect("the CAS holds the row");
    assert_eq!(descriptor.byte_size.value(), plaintext.len() as u64);
    assert_eq!(descriptor.kek_id, "kek-ctx-private");

    // Digest-addressed and whole: the read takes the address and the class,
    // nothing else, and returns exactly what went in.
    let read = cas
        .get(&record.digest, ContentClass::Private)
        .await
        .expect("get");
    assert_eq!(read, plaintext);

    // A digest nothing classified is a complete answer, not an error message
    // about paths.
    let absent = address_of(b"never stored");
    assert_eq!(
        cas.get(&absent, ContentClass::Private).await,
        Err(ContextCasError::NotFound)
    );
    assert_eq!(cas.describe(&absent).await, Ok(None));

    // The identical classified write is a dedup hit that writes nothing new.
    let (again, deduped) = cas
        .put(
            private_manifest(),
            "application/json".to_owned(),
            &plaintext,
        )
        .await
        .expect("idempotent put");
    assert!(deduped);
    assert_eq!(again.digest, record.digest);

    // The same bytes under a DIFFERENT classification are a refused write, and
    // the refusal names both sides. One digest, one classification, forever.
    let other = classes(
        ContentClass::Private,
        RedactionClass::None,
        RetentionClass::Permanent,
    );
    match cas
        .put(other, "application/json".to_owned(), &plaintext)
        .await
    {
        Err(ContextCasError::ClassMismatch {
            stored, requested, ..
        }) => {
            assert_eq!(stored, private_manifest());
            assert_eq!(requested, other);
        }
        wrong => panic!("a reclassifying put must be refused: {wrong:?}"),
    }

    teardown(&maintenance, &name, &root).await;
    drop(store);
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_blob_sealed_under_one_class_kek_refuses_to_open_under_another() {
    let maintenance = maintenance_pool().await;
    let (name, store) = raw_store(&maintenance, "ctxkek", 8).await;
    let root = fresh_root("ctxkek");
    let cas = adapter(store.pool(), &root).await;

    let plaintext = b"private content".to_vec();
    let (record, _) = cas
        .put(private_manifest(), "text/plain".to_owned(), &plaintext)
        .await
        .expect("put");

    // The metadata boundary: a read under the other content class is refused
    // before a byte is read, and the refusal names both classes.
    match cas.get(&record.digest, ContentClass::Conformance).await {
        Err(ContextCasError::WrongContentClass {
            stored, requested, ..
        }) => {
            assert_eq!(stored, ContentClass::Private);
            assert_eq!(requested, ContentClass::Conformance);
        }
        wrong => panic!("a cross-class read must be refused: {wrong:?}"),
    }

    // The cryptographic boundary behind it — the PLAN's swapped-KEK arm. A
    // second adapter whose ring carries the other class's key behind this
    // label fails AUTHENTICATION on the wrapped DEK: never a mis-decrypt, and
    // deliberately indistinguishable from a tampered header.
    let mis_keyed = PgContextCasStore::open(store.pool().clone(), root.clone(), &swapped_ring())
        .await
        .expect("open the mis-keyed store");
    match mis_keyed.get(&record.digest, ContentClass::Private).await {
        Err(ContextCasError::Blob(BlobError::Integrity(message))) => {
            assert!(message.contains("wrong kek or altered header"), "{message}");
        }
        wrong => panic!("a swapped class key must fail authentication: {wrong:?}"),
    }

    teardown(&maintenance, &name, &root).await;
    drop(store);
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn every_class_axis_is_a_closed_set_at_insert() {
    let maintenance = maintenance_pool().await;
    let (name, store) = raw_store(&maintenance, "ctxclosed", 8).await;

    // The positive control first: a row wearing legal tokens on every axis
    // lands. Without it the three refusals below could be a table that
    // refuses everything.
    let legal = address_of(b"legal");
    sqlx::query(
        "INSERT INTO gwk.context_blob (digest, content_class, redaction_class, retention_class) \
         VALUES ($1, 'private', 'redacted', 'manifest')",
    )
    .bind(legal.as_str())
    .execute(store.pool())
    .await
    .expect("a legal classification lands");

    // One token outside each closed set, refused BY THE DATABASE — the Rust
    // enums cannot even spell these, so the DDL CHECK is the arm under test.
    for (column, insert) in [
        (
            "content_class",
            "INSERT INTO gwk.context_blob (digest, content_class, redaction_class, retention_class) \
             VALUES ($1, 'secret', 'redacted', 'manifest')",
        ),
        (
            "redaction_class",
            "INSERT INTO gwk.context_blob (digest, content_class, redaction_class, retention_class) \
             VALUES ($1, 'private', 'partial', 'manifest')",
        ),
        (
            "retention_class",
            "INSERT INTO gwk.context_blob (digest, content_class, redaction_class, retention_class) \
             VALUES ($1, 'private', 'redacted', 'weekly')",
        ),
    ] {
        let digest = address_of(column.as_bytes());
        let refused = sqlx::query(insert)
            .bind(digest.as_str())
            .execute(store.pool())
            .await
            .expect_err(column);
        assert!(
            refused.to_string().contains(column),
            "{column}: the refusal must come from that axis's CHECK: {refused}"
        );
    }

    // And the append-only guard, which is what makes a classification a fact
    // rather than a mutable setting: UPDATE and DELETE are refused by the
    // trigger even for the superuser this test connects as.
    for statement in [
        "UPDATE gwk.context_blob SET retention_class = 'permanent' WHERE digest = $1",
        "DELETE FROM gwk.context_blob WHERE digest = $1",
    ] {
        let refused = sqlx::query(statement)
            .bind(legal.as_str())
            .execute(store.pool())
            .await
            .expect_err("append-only");
        assert!(
            refused.to_string().contains("append-only"),
            "{statement}: {refused}"
        );
    }

    let root = fresh_root("ctxclosed");
    teardown(&maintenance, &name, &root).await;
    drop(store);
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn retention_reclaims_exactly_what_no_class_window_or_pin_protects() {
    let maintenance = maintenance_pool().await;
    let (name, store) = raw_store(&maintenance, "ctxsweep", 8).await;
    let root = fresh_root("ctxsweep");
    let cas = adapter(store.pool(), &root).await;

    // The sweeping store: the MAIN kernel store over the same root and
    // database, with a 30-day manifest window configured — the test-side twin
    // of GWK_CONTEXT_RETENTION_DAYS_MANIFEST.
    let sweeper_config = BlobConfig::new(
        root.clone(),
        common::TEST_KEK,
        common::TEST_KEK_ID.to_owned(),
    )
    .expect("blob config")
    .with_context_retention(vec![(RetentionClass::Manifest, 30)]);
    let sweeper = PgBlobStore::open(store.pool().clone(), sweeper_config)
        .await
        .expect("open the sweeping store");

    // Five classified blobs, one per arm. Ages are seeded as pre-claimed
    // classification rows (the table is append-only, so nothing can backdate
    // a claim after the fact); the puts then converge on them.
    let expired = bytes(64);
    let fresh = bytes(65);
    let permanent = bytes(66);
    let unconfigured = bytes(67);
    let pinned = bytes(68);

    seed_aged_claim(store.pool(), &address_of(&expired), private_manifest(), 40).await;
    seed_aged_claim(
        store.pool(),
        &address_of(&permanent),
        classes(
            ContentClass::Private,
            RedactionClass::Redacted,
            RetentionClass::Permanent,
        ),
        400,
    )
    .await;
    seed_aged_claim(
        store.pool(),
        &address_of(&unconfigured),
        classes(
            ContentClass::Private,
            RedactionClass::Redacted,
            RetentionClass::Release,
        ),
        400,
    )
    .await;
    seed_aged_claim(store.pool(), &address_of(&pinned), private_manifest(), 40).await;

    for (plaintext, c) in [
        (&expired, private_manifest()),
        (&fresh, private_manifest()),
        (
            &permanent,
            classes(
                ContentClass::Private,
                RedactionClass::Redacted,
                RetentionClass::Permanent,
            ),
        ),
        (
            &unconfigured,
            classes(
                ContentClass::Private,
                RedactionClass::Redacted,
                RetentionClass::Release,
            ),
        ),
        (&pinned, private_manifest()),
    ] {
        cas.put(c, "application/octet-stream".to_owned(), plaintext)
            .await
            .expect("put");
    }
    cas.pin(&address_of(&pinned), &EvidenceId::new("ev-pin"))
        .await
        .expect("pin");

    // An expired classified blob that ALSO has an ordinary evidence row: the
    // class, not the evidence kind, decides how long the bytes stay — the
    // carve-out that makes retention classes mean anything at all.
    sqlx::query(
        "INSERT INTO gwk.evidence (id, kind, ref, digest, byte_size) VALUES ($1, 'diff', $2, $2, 1)",
    )
    .bind("ev-diff")
    .bind(address_of(&expired).as_str())
    .execute(store.pool())
    .await
    .expect("insert an ordinary evidence row");

    let swept = sweeper.sweep().await.expect("sweep");
    assert_eq!(
        swept,
        vec![address_of(&expired)],
        "exactly the expired, unpinned manifest-class blob is reclaimed"
    );

    // The four survivors, each for its own named reason.
    for (why, plaintext) in [
        ("inside its window", &fresh),
        ("permanent has no window", &permanent),
        (
            "its class has no CONFIGURED window — fail safe",
            &unconfigured,
        ),
        ("pinned as evidence, expiry notwithstanding", &pinned),
    ] {
        let read = cas
            .get(&address_of(plaintext), ContentClass::Private)
            .await
            .unwrap_or_else(|e| panic!("{why}: {e}"));
        assert_eq!(&read, plaintext, "{why}");
    }

    // The reclaimed blob's CLASSIFICATION survives as the audit record: what
    // class the content was is exactly what a retention audit asks after the
    // bytes are gone.
    let record = cas
        .describe(&address_of(&expired))
        .await
        .expect("describe")
        .expect("the classification outlives the bytes");
    assert_eq!(record.classes, private_manifest());
    assert!(record.blob.is_none(), "the CAS row was reclaimed");

    // Releasing the pin releases the last protection: the pin override, shown
    // in both directions rather than asserted once.
    cas.unpin(&address_of(&pinned), &EvidenceId::new("ev-pin"))
        .await
        .expect("unpin");
    let swept = sweeper.sweep().await.expect("second sweep");
    assert_eq!(
        swept,
        vec![address_of(&pinned)],
        "the unpinned expired blob is reclaimed by the very next sweep"
    );

    teardown(&maintenance, &name, &root).await;
    drop(store);
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn the_classification_row_carries_no_reconstructable_content() {
    // RED bullet 5's shape floor: the table is classification and accounting,
    // and this pins the COLUMN SET so a content-bearing column cannot arrive
    // quietly. Count first, then names.
    let maintenance = maintenance_pool().await;
    let (name, store) = raw_store(&maintenance, "ctxshape", 8).await;

    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name FROM information_schema.columns \
         WHERE table_schema = 'gwk' AND table_name = 'context_blob' ORDER BY column_name",
    )
    .fetch_all(store.pool())
    .await
    .expect("read the column set");
    assert_eq!(
        columns,
        [
            "content_class",
            "created_at",
            "digest",
            "redaction_class",
            "retention_class"
        ],
        "the classification table changed shape — nothing in it may carry content"
    );

    let root = fresh_root("ctxshape");
    teardown(&maintenance, &name, &root).await;
    drop(store);
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_pre_claimed_classification_without_bytes_is_honest_and_completable() {
    // The crash window the put ordering chooses: classification claimed,
    // bytes never landed. `describe` reports it exactly, `get` refuses at the
    // blob layer, and a retried put completes it.
    let maintenance = maintenance_pool().await;
    let (name, store) = raw_store(&maintenance, "ctxclaim", 8).await;
    let root = fresh_root("ctxclaim");
    let cas = adapter(store.pool(), &root).await;

    let plaintext = b"claimed then crashed".to_vec();
    let digest = address_of(&plaintext);
    seed_aged_claim(store.pool(), &digest, private_manifest(), 0).await;

    let record = cas
        .describe(&digest)
        .await
        .expect("describe")
        .expect("the claim is visible");
    assert!(record.blob.is_none(), "no bytes have landed");
    assert_eq!(
        cas.get(&digest, ContentClass::Private).await,
        Err(ContextCasError::NotFound),
        "a claim without bytes reads as not found, never as empty content"
    );

    let (record, deduped) = cas
        .put(private_manifest(), "text/plain".to_owned(), &plaintext)
        .await
        .expect("the retry completes the claim");
    assert!(!deduped, "the retry wrote real bytes");
    assert!(record.blob.is_some());
    assert_eq!(
        cas.get(&digest, ContentClass::Private).await.expect("get"),
        plaintext
    );

    teardown(&maintenance, &name, &root).await;
    drop(store);
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn bytes_already_sealed_under_a_foreign_key_domain_are_refused() {
    // A content collision with a kernel-internal blob: identical bytes already
    // in the CAS under the main store's KEK. Classifying them would promise a
    // class KEK that cannot open them, so the put is refused before any
    // classification is claimed — and the refusal names the nonsecret label
    // actually standing.
    let maintenance = maintenance_pool().await;
    let (name, store) = raw_store(&maintenance, "ctxforeign", 8).await;
    let root = fresh_root("ctxforeign");
    let cas = adapter(store.pool(), &root).await;

    let kernel_config = BlobConfig::new(
        root.clone(),
        common::TEST_KEK,
        common::TEST_KEK_ID.to_owned(),
    )
    .expect("blob config");
    let kernel_store = PgBlobStore::open(store.pool().clone(), kernel_config)
        .await
        .expect("open the kernel store");

    let plaintext = b"shared bytes".to_vec();
    let upload = kernel_store
        .begin(
            "text/plain".to_owned(),
            ByteCount::new(plaintext.len() as u64),
        )
        .await
        .expect("begin");
    kernel_store
        .write_chunk(&upload, 0, &plaintext)
        .await
        .expect("chunk");
    kernel_store
        .commit(upload, address_of(&plaintext))
        .await
        .expect("commit");

    match cas
        .put(private_manifest(), "text/plain".to_owned(), &plaintext)
        .await
    {
        Err(ContextCasError::ForeignKeyDomain { stored_kek_id, .. }) => {
            assert_eq!(stored_kek_id, common::TEST_KEK_ID);
        }
        wrong => panic!("a foreign-domain collision must be refused: {wrong:?}"),
    }
    // And nothing was claimed on the way to the refusal.
    assert_eq!(cas.describe(&address_of(&plaintext)).await, Ok(None));

    teardown(&maintenance, &name, &root).await;
    drop(store);
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn the_container_integrity_canary_holds_through_the_adapter() {
    // R17: the container bytes and their AEAD are untouched, so tampering a
    // byte on disk fails authentication through the adapter exactly as it does
    // through the plain store. A canary for shipped behaviour, not this task's
    // RED — a fresh arm here could not fail for the intended missing
    // behaviour, because the behaviour was never missing.
    let maintenance = maintenance_pool().await;
    let (name, store) = raw_store(&maintenance, "ctxtamper", 8).await;
    let root = fresh_root("ctxtamper");
    let cas = adapter(store.pool(), &root).await;

    let plaintext = bytes(256);
    let (record, _) = cas
        .put(
            private_manifest(),
            "application/octet-stream".to_owned(),
            &plaintext,
        )
        .await
        .expect("put");

    let hex = record.digest.digest_hex();
    let path = root
        .join("blobs")
        .join(&hex[0..2])
        .join(&hex[2..4])
        .join(hex);
    let mut container = std::fs::read(&path).expect("read the container");
    let last = container.len() - 1;
    container[last] ^= 0x01;
    std::fs::write(&path, container).expect("tamper the container");

    match cas.get(&record.digest, ContentClass::Private).await {
        Err(ContextCasError::Blob(BlobError::Integrity(_))) => {}
        wrong => panic!("a tampered container must fail authentication: {wrong:?}"),
    }

    teardown(&maintenance, &name, &root).await;
    drop(store);
}
