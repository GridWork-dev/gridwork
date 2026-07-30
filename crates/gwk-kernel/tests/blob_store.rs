//! Certifies the blob store: uploads, dedup, ranged reads, pins, sweep, shred,
//! and rotation.
//!
//! What only a real database and a real filesystem can show: that a blob
//! survives the round trip through chunked upload and seek-by-chunk reads; that
//! dedup is decided by digest AND media type; that sweep asks the LOG which
//! blobs are still referenced and that an evidence pin overrides it; that
//! crypto-shred is permanent even against someone who kept the plaintext; and
//! that a rotation changes every wrapped key without touching one byte of
//! ciphertext.
//!
//! `#[ignore]` because it needs a server — see `tests/common/mod.rs`.

mod common;

use common::{
    TEST_KEK, address_of, blob_store, drop_database, maintenance_pool, put, put_dedup, raw_store,
    read_all,
};
use gwk_domain::blob::{BLOB_CHUNK_BYTES, BlobAddress};
use gwk_domain::envelope::{Actor, EventEnvelope, Origin, PayloadRef};
use gwk_domain::ids::{
    AggregateId, BlobUploadId, ByteCount, EventId, EvidenceId, ProjectId, Seq, Timestamp,
};
use gwk_domain::port::{BlobError, BlobStore, EventStore};

/// Plaintext that is not compressible into a lucky pattern, so a chunk landing
/// at the wrong offset shows up as wrong bytes rather than the same bytes.
fn bytes(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 31 + i / 97) % 251) as u8).collect()
}

/// One event whose payload points at `address` — what makes a blob referenced.
fn referencing_event(address: &BlobAddress, size: u64) -> EventEnvelope {
    EventEnvelope {
        event_id: EventId::new("evt-1"),
        project_id: ProjectId::new("p"),
        aggregate_type: "task".into(),
        aggregate_id: AggregateId::new("t1"),
        aggregate_version: 1,
        event_type: "artifact_recorded".into(),
        schema_version: 1,
        global_sequence: Seq::new(0),
        occurred_at: Timestamp::new("2026-07-28T00:00:00Z"),
        appended_at: Timestamp::new("2026-07-28T00:00:00Z"),
        actor: Actor {
            kind: "kernel".into(),
            id: None,
        },
        origin: Origin {
            system: "gw".into(),
            r#ref: None,
        },
        causation_id: None,
        correlation_id: None,
        idempotency_key: None,
        payload: serde_json::json!({}),
        payload_ref: Some(PayloadRef {
            digest: address.as_str().to_owned(),
            media_type: "application/json".to_owned(),
            byte_size: ByteCount::new(size),
            retention_class: None,
            evidence_pin: None,
        }),
    }
}

async fn teardown(maintenance: &sqlx::PgPool, name: &str, root: &std::path::Path) {
    drop_database(maintenance, name).await;
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_blob_survives_chunked_upload_and_every_ranged_read_of_it() {
    let maintenance = maintenance_pool().await;
    let (name, store) = raw_store(&maintenance, "blobround", 8).await;
    let (root, blobs) = blob_store(&store, "blobround").await;

    // Three container chunks, the last one short — every boundary case a
    // ranged read has to land on lives in this one blob.
    let plaintext = bytes(BLOB_CHUNK_BYTES * 2 + 7);
    let size = plaintext.len() as u64;
    let address = put(&blobs, &plaintext, "application/octet-stream").await;
    assert_eq!(address, address_of(&plaintext));

    let descriptor = blobs
        .stat(&address)
        .await
        .expect("stat")
        .expect("committed");
    assert_eq!(descriptor.byte_size.value(), size);
    assert_eq!(descriptor.media_type, "application/octet-stream");
    assert_eq!(descriptor.kek_id, common::TEST_KEK_ID);
    assert!(!descriptor.pinned && !descriptor.tombstoned);

    assert_eq!(read_all(&blobs, &address, size).await, plaintext);

    let chunk = BLOB_CHUNK_BYTES as u64;
    for (why, offset, length) in [
        ("the very start", 0u64, 10u64),
        ("mid first chunk", 500, 100),
        ("across the first boundary", chunk - 5, 10),
        ("exactly a chunk start", chunk, 16),
        ("across the second boundary", chunk * 2 - 3, 6),
        ("the short final chunk", chunk * 2, 7),
        ("the last byte", size - 1, 1),
    ] {
        let got = blobs
            .read(&address, ByteCount::new(offset), ByteCount::new(length))
            .await
            .unwrap_or_else(|e| panic!("{why}: {e}"));
        let want = &plaintext[offset as usize..(offset + length) as usize];
        assert_eq!(got, want, "{why}");
    }

    // Clamped, not refused: a huge length is a request for as much as the store
    // will give, and the ceiling is one chunk — the same unit the wire ships in.
    let big = blobs
        .read(&address, ByteCount::new(0), ByteCount::new(u64::MAX))
        .await
        .expect("clamped read");
    assert_eq!(big.len(), BLOB_CHUNK_BYTES);
    assert_eq!(big, plaintext[..BLOB_CHUNK_BYTES]);
    // A range that starts past the end is empty, not an error: it is a legal
    // question with no bytes in the answer.
    for offset in [size, size + 1, u64::MAX] {
        let past = blobs
            .read(&address, ByteCount::new(offset), ByteCount::new(64))
            .await
            .expect("past the end");
        assert!(past.is_empty(), "offset {offset}");
    }
    // A length clamped by the blob's own end, not padded to what was asked.
    let tail = blobs
        .read(&address, ByteCount::new(size - 3), ByteCount::new(999))
        .await
        .expect("tail");
    assert_eq!(tail, plaintext[(size - 3) as usize..]);

    // An empty blob is a real blob: one sealed chunk, its own address.
    let empty = put(&blobs, b"", "text/plain").await;
    assert_eq!(
        blobs
            .stat(&empty)
            .await
            .expect("stat")
            .expect("committed")
            .byte_size
            .value(),
        0
    );
    assert!(
        blobs
            .read(&empty, ByteCount::new(0), ByteCount::new(16))
            .await
            .expect("read empty")
            .is_empty()
    );

    teardown(&maintenance, &name, &root).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn dedup_needs_the_digest_and_the_media_type_to_agree() {
    let maintenance = maintenance_pool().await;
    let (name, store) = raw_store(&maintenance, "blobdedup", 8).await;
    let (root, blobs) = blob_store(&store, "blobdedup").await;

    let plaintext = b"the same bytes twice".to_vec();
    let (address, first) = put_dedup(&blobs, &plaintext, "text/plain").await;
    assert!(!first, "the first commit stores");
    let (again, second) = put_dedup(&blobs, &plaintext, "text/plain").await;
    assert!(second, "the second commit dedups");
    assert_eq!(again, address);

    // Same bytes, different media type. It cannot be a SECOND blob — the
    // address is the digest alone, so both would answer to it — and dedup
    // requires all three to match. It is the caller disagreeing with the store
    // about what these bytes are, and it is refused.
    let upload = blobs
        .begin(
            "application/json".to_owned(),
            ByteCount::new(plaintext.len() as u64),
        )
        .await
        .expect("begin");
    blobs
        .write_chunk(&upload, 0, &plaintext)
        .await
        .expect("chunk");
    let err = blobs
        .commit(upload, address.clone())
        .await
        .expect_err("a conflicting media type must be refused");
    assert!(
        matches!(&err, BlobError::Integrity(reason) if reason.contains("text/plain")),
        "{err:?}"
    );
    // The stored blob is untouched by the refusal.
    assert_eq!(
        blobs
            .stat(&address)
            .await
            .expect("stat")
            .expect("still there")
            .media_type,
        "text/plain"
    );

    teardown(&maintenance, &name, &root).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn an_upload_is_bounded_by_the_order_and_the_size_it_declared() {
    let maintenance = maintenance_pool().await;
    let (name, store) = raw_store(&maintenance, "blobupload", 8).await;
    let (root, blobs) = blob_store(&store, "blobupload").await;

    let upload = blobs
        .begin("text/plain".to_owned(), ByteCount::new(10))
        .await
        .expect("begin");
    // A gap and a repeat are both refused rather than reordered, because a
    // store that accepted either would commit a digest over bytes the caller
    // never sent in that order.
    for bad in [1u32, 2, 7] {
        let err = blobs
            .write_chunk(&upload, bad, b"xx")
            .await
            .expect_err("out of order");
        assert!(matches!(err, BlobError::Integrity(_)), "{bad}: {err:?}");
    }
    blobs
        .write_chunk(&upload, 0, b"12345")
        .await
        .expect("chunk 0");
    let err = blobs
        .write_chunk(&upload, 0, b"12345")
        .await
        .expect_err("a repeat is not a retry once it landed");
    assert!(matches!(err, BlobError::Integrity(_)), "{err:?}");

    // The declared size is a budget, not a hint: without this an upload fills
    // the disk regardless of what it announced.
    let err = blobs
        .write_chunk(&upload, 1, &[0u8; 6])
        .await
        .expect_err("past the declared size");
    assert!(
        matches!(&err, BlobError::Integrity(reason) if reason.contains("declared")),
        "{err:?}"
    );

    // Committing short is refused too — the digest would be over a prefix.
    let err = blobs
        .commit(upload.clone(), address_of(b"12345"))
        .await
        .expect_err("short of the declared size");
    assert!(
        matches!(&err, BlobError::Integrity(reason) if reason.contains("5 of the 10")),
        "{err:?}"
    );

    // Finish it honestly, but claim the wrong address.
    blobs
        .write_chunk(&upload, 1, b"67890")
        .await
        .expect("chunk 1");
    let wrong = address_of(b"something else entirely");
    let err = blobs
        .commit(upload.clone(), wrong.clone())
        .await
        .expect_err("a claimed address that is not the digest");
    match err {
        BlobError::DigestMismatch { expected, actual } => {
            assert_eq!(expected, wrong);
            assert_eq!(actual, address_of(b"1234567890"));
        }
        other => panic!("expected DigestMismatch, got {other:?}"),
    }
    // The failed commit did not consume the upload; the honest one still works.
    let (descriptor, deduped) = blobs
        .commit(upload.clone(), address_of(b"1234567890"))
        .await
        .expect("commit");
    assert!(!deduped);
    assert_eq!(descriptor.byte_size.value(), 10);

    // The upload is gone once it committed, and abort says so.
    let err = blobs.abort(upload).await.expect_err("already committed");
    assert!(matches!(err, BlobError::NotFound), "{err:?}");

    // An id this store never minted names nothing — including one built to walk
    // out of the upload directory.
    for forged in ["../../etc/passwd", "", &"a".repeat(31), "NOTHEX"] {
        let err = blobs
            .write_chunk(&BlobUploadId::new(forged), 0, b"x")
            .await
            .expect_err("a forged upload id");
        assert!(matches!(err, BlobError::NotFound), "{forged:?}: {err:?}");
    }

    // Aborting a real upload takes its staging file with it.
    let doomed = blobs
        .begin("text/plain".to_owned(), ByteCount::new(4))
        .await
        .expect("begin");
    blobs.write_chunk(&doomed, 0, b"abcd").await.expect("chunk");
    let staged = root.join("uploads").join(doomed.as_str());
    assert!(staged.exists());
    blobs.abort(doomed).await.expect("abort");
    assert!(!staged.exists(), "abort must take the staging file with it");

    teardown(&maintenance, &name, &root).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn sweep_reclaims_what_the_log_stopped_pointing_at() {
    let maintenance = maintenance_pool().await;
    let (name, store) = raw_store(&maintenance, "blobsweep", 8).await;
    let (root, blobs) = blob_store(&store, "blobsweep").await;

    let referenced = bytes(64);
    let orphan = bytes(65);
    let pinned = bytes(66);
    let kept = put(&blobs, &referenced, "application/json").await;
    let loose = put(&blobs, &orphan, "application/json").await;
    let held = put(&blobs, &pinned, "application/json").await;

    // One event points at the first blob. This is the whole question sweep
    // asks, and it asks it of the LOG — not of a reference count the store
    // maintains, which could drift from the log it is supposed to describe.
    store
        .append(
            0,
            None,
            vec![referencing_event(&kept, referenced.len() as u64)],
        )
        .await
        .expect("append");
    blobs
        .pin(&held, &EvidenceId::new("ev-1"))
        .await
        .expect("pin");

    let swept = blobs.sweep().await.expect("sweep");
    assert_eq!(swept, vec![loose.clone()]);
    assert!(blobs.stat(&loose).await.expect("stat").is_none());
    assert!(
        !root
            .join("blobs")
            .join(&loose.digest_hex()[0..2])
            .join(&loose.digest_hex()[2..4])
            .join(loose.digest_hex())
            .exists()
    );
    // Both survivors are still readable, for their own separate reasons.
    assert_eq!(
        read_all(&blobs, &kept, referenced.len() as u64).await,
        referenced
    );
    assert_eq!(read_all(&blobs, &held, pinned.len() as u64).await, pinned);

    // Sweep is idempotent: nothing left to reclaim, nothing reported.
    assert!(blobs.sweep().await.expect("second sweep").is_empty());

    // Release the pin and the blob it was holding becomes reclaimable — one
    // pin releasing is enough here because only one was ever taken.
    blobs
        .unpin(&held, &EvidenceId::new("ev-1"))
        .await
        .expect("unpin");
    assert_eq!(blobs.sweep().await.expect("sweep"), vec![held.clone()]);
    // The referenced one is never in reach of a sweep at all.
    assert!(blobs.stat(&kept).await.expect("stat").is_some());

    teardown(&maintenance, &name, &root).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_pin_survives_every_release_but_the_last() {
    let maintenance = maintenance_pool().await;
    let (name, store) = raw_store(&maintenance, "blobpin", 8).await;
    let (root, blobs) = blob_store(&store, "blobpin").await;

    let plaintext = bytes(32);
    let address = put(&blobs, &plaintext, "application/json").await;
    for evidence in ["ev-a", "ev-b"] {
        blobs
            .pin(&address, &EvidenceId::new(evidence))
            .await
            .expect("pin");
    }
    // Pinning twice under one evidence id is the state the caller asked for,
    // not a second hold — otherwise a retried pin would need a matching extra
    // release that nobody knows to send.
    blobs
        .pin(&address, &EvidenceId::new("ev-a"))
        .await
        .expect("pin again");
    assert!(
        blobs
            .stat(&address)
            .await
            .expect("stat")
            .expect("there")
            .pinned
    );

    // Evidence outranks retention: neither sweep nor shred may touch it.
    assert!(blobs.sweep().await.expect("sweep").is_empty());
    assert!(matches!(
        blobs.shred(&address).await.expect_err("pinned"),
        BlobError::Pinned
    ));

    blobs
        .unpin(&address, &EvidenceId::new("ev-a"))
        .await
        .expect("unpin a");
    // Releasing one hold is not releasing the set.
    assert!(blobs.sweep().await.expect("sweep").is_empty());
    // Releasing a hold nobody took is the state asked for, not an error.
    blobs
        .unpin(&address, &EvidenceId::new("ev-never"))
        .await
        .expect("unpin an absent hold");

    blobs
        .unpin(&address, &EvidenceId::new("ev-b"))
        .await
        .expect("unpin b");
    assert!(
        !blobs
            .stat(&address)
            .await
            .expect("stat")
            .expect("there")
            .pinned
    );
    blobs.shred(&address).await.expect("shred");

    teardown(&maintenance, &name, &root).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn crypto_shred_is_permanent_even_against_someone_holding_the_plaintext() {
    let maintenance = maintenance_pool().await;
    let (name, store) = raw_store(&maintenance, "blobshred", 8).await;
    let (root, blobs) = blob_store(&store, "blobshred").await;

    let plaintext = bytes(4096);
    let address = put(&blobs, &plaintext, "application/json").await;
    let path = root
        .join("blobs")
        .join(&address.digest_hex()[0..2])
        .join(&address.digest_hex()[2..4])
        .join(address.digest_hex());
    assert!(path.exists());

    blobs.shred(&address).await.expect("shred");
    assert!(!path.exists(), "shred removes the ciphertext after the key");

    // Reads fail permanently, and `stat` still says the blob EXISTED — a
    // retention audit needs "destroyed" and "never written" to be different
    // answers, so this is deliberately not `Ok(None)`.
    for err in [
        blobs
            .read(&address, ByteCount::new(0), ByteCount::new(16))
            .await
            .expect_err("read a shredded blob"),
        blobs
            .stat(&address)
            .await
            .expect_err("stat a shredded blob"),
    ] {
        assert!(matches!(err, BlobError::Tombstoned), "{err:?}");
    }
    // Terminal, so re-running it is the state the caller wants.
    blobs.shred(&address).await.expect("shred again");

    // Re-uploading the identical bytes does NOT resurrect the address. Shred is
    // a retention decision about an address; re-presenting the plaintext is not
    // an appeal of it, or anyone who kept a copy could undo a deletion the
    // audit log calls final.
    let upload = blobs
        .begin(
            "application/json".to_owned(),
            ByteCount::new(plaintext.len() as u64),
        )
        .await
        .expect("begin");
    blobs
        .write_chunk(&upload, 0, &plaintext)
        .await
        .expect("chunk");
    let err = blobs
        .commit(upload, address.clone())
        .await
        .expect_err("a shredded address stays shredded");
    assert!(matches!(err, BlobError::Tombstoned), "{err:?}");

    // Sweep never turns it back into a readable blob either — the row is
    // excluded from the candidate set precisely so its tombstone survives.
    assert!(blobs.sweep().await.expect("sweep").is_empty());
    assert!(matches!(
        blobs.stat(&address).await.expect_err("still tombstoned"),
        BlobError::Tombstoned
    ));

    // A blob that never existed is a different answer from a destroyed one.
    assert!(
        blobs
            .stat(&address_of(b"never uploaded"))
            .await
            .expect("stat")
            .is_none()
    );
    assert!(matches!(
        blobs
            .shred(&address_of(b"never uploaded"))
            .await
            .expect_err("no such blob"),
        BlobError::NotFound
    ));

    teardown(&maintenance, &name, &root).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_tampered_container_never_returns_bytes() {
    let maintenance = maintenance_pool().await;
    let (name, store) = raw_store(&maintenance, "blobtamper", 8).await;
    let (root, blobs) = blob_store(&store, "blobtamper").await;

    let plaintext = bytes(BLOB_CHUNK_BYTES + 128);
    let address = put(&blobs, &plaintext, "application/json").await;
    let path = root
        .join("blobs")
        .join(&address.digest_hex()[0..2])
        .join(&address.digest_hex()[2..4])
        .join(address.digest_hex());

    let original = std::fs::read(&path).expect("read container");
    let last_chunk = BLOB_CHUNK_BYTES as u64;
    // Every region, read through a range that actually touches it: the
    // authenticated header, the first chunk's ciphertext, and the last chunk's
    // tag. All three are covered by a tag, so all three fail.
    for (why, at, offset) in [
        ("the header", 16usize, 0u64),
        ("the first chunk", original.len() / 4, 0),
        ("the final chunk's tag", original.len() - 1, last_chunk),
    ] {
        let mut edited = original.clone();
        edited[at] ^= 0xff;
        std::fs::write(&path, &edited).expect("write");
        let err = blobs
            .read(&address, ByteCount::new(offset), ByteCount::new(64))
            .await
            .err()
            .unwrap_or_else(|| panic!("{why}: a tampered container must not read"));
        assert!(matches!(err, BlobError::Integrity(_)), "{why}: {err:?}");
    }

    // The blast radius is the chunks a range touches, and no more. A read of
    // the first chunk still succeeds while the LAST one is corrupt — which is
    // the honest consequence of authenticating per chunk instead of per file,
    // and the reason a caller that needs the whole blob has to read the whole
    // blob rather than trusting a spot check.
    let mut edited = original.clone();
    let end = edited.len() - 1;
    edited[end] ^= 0xff;
    std::fs::write(&path, &edited).expect("write");
    assert_eq!(
        blobs
            .read(&address, ByteCount::new(0), ByteCount::new(64))
            .await
            .expect("an untouched chunk still reads"),
        plaintext[..64]
    );

    // Truncation is caught by the same tag: the last chunk was sealed AS last,
    // so a shortened file arrives with a middle chunk in the final position —
    // and when the cut is deep enough that the chunk is not there at all, the
    // header still says how many there should be, so running off the end is an
    // integrity failure rather than an I/O one.
    std::fs::write(&path, &original[..original.len() - 4096]).expect("truncate");
    let err = blobs
        .read(&address, ByteCount::new(last_chunk), ByteCount::new(64))
        .await
        .expect_err("a truncated container must not read");
    assert!(matches!(err, BlobError::Integrity(_)), "{err:?}");

    std::fs::write(&path, &original).expect("restore");
    assert_eq!(
        read_all(&blobs, &address, plaintext.len() as u64).await,
        plaintext
    );

    teardown(&maintenance, &name, &root).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn rotation_replaces_every_wrapped_key_and_no_ciphertext() {
    let maintenance = maintenance_pool().await;
    let (name, store) = raw_store(&maintenance, "blobrotate", 8).await;
    let (root, blobs) = blob_store(&store, "blobrotate").await;

    let blobs_in = [bytes(100), bytes(BLOB_CHUNK_BYTES + 1), Vec::new()];
    let mut addresses = Vec::new();
    for (n, plaintext) in blobs_in.iter().enumerate() {
        addresses.push(put(&blobs, plaintext, &format!("application/x-{n}")).await);
    }
    let containers: Vec<Vec<u8>> = addresses
        .iter()
        .map(|a| {
            std::fs::read(
                root.join("blobs")
                    .join(&a.digest_hex()[0..2])
                    .join(&a.digest_hex()[2..4])
                    .join(a.digest_hex()),
            )
            .expect("read container")
        })
        .collect();

    // Shredded blobs are skipped: there is no key left to rewrap, and minting
    // one would be un-shredding.
    let doomed = put(&blobs, &bytes(7), "application/json").await;
    blobs.shred(&doomed).await.expect("shred");

    let mut new_kek = TEST_KEK;
    new_kek[0] ^= 0xff;
    assert_eq!(blobs.rewrap_all(&new_kek).await.expect("rewrap"), 3);

    // Not one ciphertext byte moved. That is the property the whole placement
    // of the wrapped key was chosen for.
    for (address, before) in addresses.iter().zip(&containers) {
        let after = std::fs::read(
            root.join("blobs")
                .join(&address.digest_hex()[0..2])
                .join(&address.digest_hex()[2..4])
                .join(address.digest_hex()),
        )
        .expect("read container");
        assert_eq!(&after, before, "{address} ciphertext changed");
    }

    // The old key no longer opens them. The empty blob is skipped: a zero-byte
    // range is answered before any key is touched, so it proves nothing here.
    for (address, plaintext) in addresses.iter().zip(&blobs_in) {
        if plaintext.is_empty() {
            continue;
        }
        assert!(
            blobs
                .read(address, ByteCount::new(0), ByteCount::new(8))
                .await
                .is_err(),
            "{address} still opens under the old key"
        );
    }

    // ...and a store holding the new one reads every blob back unchanged. The
    // LABEL is deliberately the same: it lives inside each container's
    // authenticated header, so relabeling would invalidate the AAD the new wrap
    // is bound to. Rotation replaces the key behind the name.
    let rotated = common::blob_store_with(&store, &root, new_kek).await;
    for (address, plaintext) in addresses.iter().zip(&blobs_in) {
        assert_eq!(
            read_all(&rotated, address, plaintext.len() as u64).await,
            *plaintext,
            "{address}"
        );
    }
    assert!(matches!(
        rotated.stat(&doomed).await.expect_err("still shredded"),
        BlobError::Tombstoned
    ));

    teardown(&maintenance, &name, &root).await;
}
