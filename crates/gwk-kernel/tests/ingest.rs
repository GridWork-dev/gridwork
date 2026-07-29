//! Ingestion: the one command that writes a row nobody named.
//!
//! Everything else in this kernel is an aggregate whose id the caller supplies.
//! An ingested record has none — `gw ingest submit --kind <kind> --file
//! <path|->` offers nowhere to put one — so its identity is derived from the
//! `(project_id, idempotency_key)` pair the envelope already had to carry. That
//! choice is what these cases are mostly about: it makes a retried submit the
//! SAME record, and it makes two submits under different keys two records even
//! when the bytes are identical, which is what a stream of health or cost
//! samples needs.
//!
//! The closed kind set is the other half. ADR 0026 makes the absence of an
//! import path load-bearing, so `import`/`migrate`/`backfill` must be refused
//! by the DATABASE and not only by the enum that failed to name them.

mod common;

use common::*;
use gwk_domain::command::KernelCommand;
use gwk_domain::envelope::{INLINE_PAYLOAD_MAX_BYTES, PayloadRef};
use gwk_domain::ids::ByteCount;
use gwk_domain::ingestion::IngestionKind;
use gwk_domain::protocol::{KernelErrorCode, KernelResult};
use gwk_kernel::store::PgEventStore;
use sqlx::Row;

fn ingest(kind: IngestionKind, payload: serde_json::Value) -> KernelCommand {
    KernelCommand::IngestRecord {
        kind,
        payload,
        payload_ref: None,
    }
}

/// Every ingested record, as `(id, kind, payload, has_ref, event_seq)`.
async fn records(store: &PgEventStore) -> Vec<(String, String, serde_json::Value, bool, String)> {
    sqlx::query(
        "SELECT id, kind, payload, payload_ref IS NOT NULL, event_seq::text \
         FROM gwk.ingested_record ORDER BY id",
    )
    .fetch_all(store.pool())
    .await
    .expect("read ingested records")
    .iter()
    .map(|row| {
        (
            row.get(0),
            row.get(1),
            row.get(2),
            row.get(3),
            row.get::<String, _>(4),
        )
    })
    .collect()
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn every_accepted_kind_reaches_its_own_row() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "ingest_kinds", 8).await;

    for kind in IngestionKind::ALL {
        apply(
            &store,
            kind.as_str(),
            ingest(*kind, serde_json::json!({ "sample": kind.as_str() })),
        )
        .await;
    }

    let rows = records(&store).await;
    assert_eq!(rows.len(), IngestionKind::ALL.len());
    // Twelve DISTINCT rows, not twelve writes onto one: the id carries the key,
    // and each kind was submitted under its own.
    let mut kinds: Vec<&str> = rows.iter().map(|(_, kind, ..)| kind.as_str()).collect();
    kinds.sort_unstable();
    let mut expected: Vec<&str> = IngestionKind::ALL.iter().map(|k| k.as_str()).collect();
    expected.sort_unstable();
    assert_eq!(kinds, expected);
    for (id, kind, payload, has_ref, seq) in &rows {
        assert_eq!(id, &format!("ingest:{PROJECT}:{kind}"));
        assert_eq!(payload, &serde_json::json!({ "sample": kind }));
        assert!(!has_ref);
        // The pointer back to the log. Ingestion writes no transition, so this
        // is the only thing that can prove the row.
        assert_ne!(seq, "0", "{id} lost its sequence");
    }

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_same_key_twice_is_one_record_and_a_different_key_is_another() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "ingest_retry", 8).await;

    let sample = ingest(
        IngestionKind::Health,
        serde_json::json!({ "probe": "tg-bridge", "ok": true }),
    );
    let first = apply(&store, "probe-1", sample.clone()).await;
    // The retry a timed-out `gw ingest submit` makes. It resolves to the same
    // aggregate, so the log answers it from the events it already holds rather
    // than recording the same reading twice.
    let retry = apply(&store, "probe-1", sample.clone()).await;
    assert_eq!(
        first.first().map(|e| e.global_sequence),
        retry.first().map(|e| e.global_sequence),
        "the retry appended a second event"
    );

    // Byte-identical content under a NEW key is a second observation, not a
    // duplicate — two identical health probes minutes apart are two facts.
    apply(&store, "probe-2", sample).await;

    let ids: Vec<String> = records(&store).await.into_iter().map(|r| r.0).collect();
    assert_eq!(
        ids,
        [
            format!("ingest:{PROJECT}:probe-1"),
            format!("ingest:{PROJECT}:probe-2")
        ]
    );

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_key_reused_for_different_content_is_a_conflict_not_a_second_row() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "ingest_conflict", 8).await;

    apply(
        &store,
        "cost-1",
        ingest(IngestionKind::Cost, serde_json::json!({ "micros": 1200 })),
    )
    .await;
    let (code, message) = refuse(
        &store,
        "cost-1",
        ingest(IngestionKind::Cost, serde_json::json!({ "micros": 9900 })),
    )
    .await;
    assert_eq!(code, KernelErrorCode::IdempotencyConflict);
    assert!(message.contains("cost-1"), "{message}");

    // The refusal left nothing behind, and the original is untouched.
    let rows = records(&store).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].2, serde_json::json!({ "micros": 1200 }));

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn content_past_the_inline_bound_has_to_travel_as_a_reference() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "ingest_bound", 8).await;

    // A graph snapshot nobody would send inline. The bound is the append path's
    // — it covers the whole event payload — so ingestion inherits it rather
    // than re-stating it as a second limit that could drift.
    let oversized = ingest(
        IngestionKind::GraphSnapshot,
        serde_json::json!({ "graph": "x".repeat(INLINE_PAYLOAD_MAX_BYTES) }),
    );
    let (code, message) = refuse(&store, "graph-inline", oversized).await;
    assert_eq!(code, KernelErrorCode::Validation);
    assert!(message.contains("payload_ref"), "{message}");
    assert!(records(&store).await.is_empty());

    // The same content as a reference, with a bounded descriptor beside it. The
    // two are not exclusive: a reader learns the shape of the snapshot without
    // fetching ninety megabytes to do it.
    apply(
        &store,
        "graph-ref",
        KernelCommand::IngestRecord {
            kind: IngestionKind::GraphSnapshot,
            payload: serde_json::json!({ "nodes": 12000, "edges": 48122 }),
            payload_ref: Some(PayloadRef {
                digest: "sha256:abc".to_owned(),
                media_type: "application/json".to_owned(),
                byte_size: ByteCount::new(91_234_567),
                retention_class: None,
                evidence_pin: None,
            }),
        },
    )
    .await;

    let rows = records(&store).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].2,
        serde_json::json!({ "nodes": 12000, "edges": 48122 })
    );
    assert!(rows[0].3, "the reference did not reach the projection");

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn there_is_no_import_kind_and_the_database_is_where_that_is_true() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "ingest_no_import", 8).await;

    // The enum cannot name it, so the refusal happens at decode — before any
    // handler runs. That is the first wall.
    let mut envelope = envelope(
        "import-1",
        &ingest(IngestionKind::Memory, serde_json::json!({})),
    );
    envelope.payload["kind"] = serde_json::json!("import");
    match store.submit(&envelope).await {
        KernelResult::Error { code, .. } => assert_eq!(code, KernelErrorCode::Validation),
        other => panic!("an import kind was accepted: {other:?}"),
    }

    // The second wall, which is the one that matters after a future refactor:
    // even a writer that got past the type cannot land the row, because the
    // CHECK is in the schema.
    let forced = sqlx::query(
        "INSERT INTO gwk.ingested_record (id, kind, payload, event_seq, ingested_at) \
         VALUES ('ingest:p:forced', 'import', '{}'::jsonb, 1, now())",
    )
    .execute(store.pool())
    .await;
    let error = forced.expect_err("the DDL admitted an import row");
    assert!(
        error.to_string().contains("ingested_record_kind_check"),
        "{error}"
    );

    assert!(records(&store).await.is_empty());
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_ingested_record_is_append_only_like_the_receipt_beside_it() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "ingest_immutable", 8).await;

    apply(
        &store,
        "insight-1",
        ingest(
            IngestionKind::Insight,
            serde_json::json!({ "text": "the checkpoint cannot be restored" }),
        ),
    )
    .await;

    // A record is a fact that arrived. Editing one would make the projection
    // disagree with the event that produced it, and the replay that rebuilds
    // this table would then be the thing that "corrupts" it.
    for statement in [
        "UPDATE gwk.ingested_record SET payload = '{}'::jsonb",
        "DELETE FROM gwk.ingested_record",
        "TRUNCATE gwk.ingested_record",
    ] {
        let error = sqlx::raw_sql(sqlx::AssertSqlSafe(statement.to_owned()))
            .execute(store.pool())
            .await
            .expect_err("append-only guard missing");
        assert!(
            error.to_string().contains("append-only"),
            "{statement}: {error}"
        );
    }

    assert_eq!(records(&store).await.len(), 1);
    drop_database(&maintenance, &name).await;
}
