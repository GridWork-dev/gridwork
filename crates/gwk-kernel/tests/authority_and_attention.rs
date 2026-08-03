//! Certifies authority evaluation, receipts, and attention against a real
//! server.
//!
//! What only a real database can show: that a page COMMITS its receipt and
//! attention item while refusing the command, that the unresolved-dedup index
//! makes a second page on the same subject a no-op rather than a second item,
//! that a revoked grant stops matching, and that `gwk.receipt`'s append-only
//! trigger holds against the kernel's own writes.
//!
//! `#[ignore]` because it needs a server — see `tests/common/mod.rs`.

mod common;

use common::{
    PROJECT, actor, apply, drop_database, envelope_as, event_count, fresh_store, maintenance_pool,
    refuse,
};
use gwk_domain::command::KernelCommand;
use gwk_domain::ids::{AttentionItemId, AuthorityGrantId, CommandId, Timestamp};
use gwk_domain::protocol::{KernelErrorCode, KernelResult};
use gwk_kernel::store::PgEventStore;
use sqlx::Row;

/// The one gated command in the risk table.
fn stop(id: &str) -> KernelCommand {
    KernelCommand::IssueCommand {
        command_id: CommandId::new(id),
        kind: "stop_attempt".to_owned(),
        targets: vec!["a-1".to_owned()],
        actor: None,
    }
}

fn grant(id: &str, scope: Option<&str>, expires_at: Option<&str>) -> KernelCommand {
    KernelCommand::GrantAuthority {
        authority_grant_id: AuthorityGrantId::new(id),
        // Must equal the submitting envelope's actor for the grant to match.
        grantee: actor("kernel"),
        action_class: "stop".to_owned(),
        scope: scope.map(str::to_owned),
        expires_at: expires_at.map(Timestamp::new),
    }
}

async fn counts(store: &PgEventStore) -> (i64, i64) {
    let receipts: i64 = sqlx::query_scalar("SELECT count(*) FROM gwk.receipt")
        .fetch_one(store.pool())
        .await
        .expect("receipts");
    let items: i64 = sqlx::query_scalar("SELECT count(*) FROM gwk.attention_item")
        .fetch_one(store.pool())
        .await
        .expect("attention items");
    (receipts, items)
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn an_ungranted_gated_command_pages_and_the_page_survives_the_refusal() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "page", 64).await;

    let (code, message) = refuse(&store, "stop", stop("c-1")).await;
    assert_eq!(code, KernelErrorCode::Authority, "{message}");
    assert!(message.contains("stop"), "{message}");

    // The command did NOT mutate: no event, no command row.
    assert_eq!(event_count(&store).await, 0);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM gwk.command")
        .fetch_one(store.pool())
        .await
        .expect("command rows");
    assert_eq!(rows, 0);

    // ...but the trail committed. This is the whole mechanism: a page whose
    // evidence rolled back is indistinguishable from a command nobody sent.
    assert_eq!(counts(&store).await, (1, 1));
    let item = sqlx::query(
        "SELECT kind, subject_ref, to_json(resolved_at) #>> '{}' AS resolved \
         FROM gwk.attention_item",
    )
    .fetch_one(store.pool())
    .await
    .expect("attention item");
    assert_eq!(item.get::<String, _>("kind"), "authority");
    assert_eq!(
        item.get::<Option<String>, _>("subject_ref").as_deref(),
        Some("command/c-1")
    );
    assert!(
        item.get::<Option<String>, _>("resolved").is_none(),
        "a fresh page is unresolved"
    );

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_second_page_on_the_same_subject_joins_the_open_item() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "pagededup", 64).await;

    for key in ["stop-1", "stop-2", "stop-3"] {
        let (code, _) = refuse(&store, key, stop("c-1")).await;
        assert_eq!(code, KernelErrorCode::Authority);
    }
    // Three refusals, three distinct receipts (each key is its own attempt),
    // ONE attention item — a page fires once per problem, not per retry.
    assert_eq!(counts(&store).await, (3, 1));

    // A different subject is a different problem.
    let (code, _) = refuse(&store, "stop-other", stop("c-2")).await;
    assert_eq!(code, KernelErrorCode::Authority);
    assert_eq!(counts(&store).await, (4, 2));

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_matching_grant_allows_and_revoking_it_pages_again() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "grant", 64).await;

    // Unscoped grant covers the whole action class.
    apply(&store, "grant", grant("g-1", None, None)).await;
    apply(&store, "stop", stop("c-1")).await;

    // Allowed commands are receipted too — every result writes one.
    let basis: String =
        sqlx::query_scalar("SELECT observed_basis FROM gwk.receipt WHERE subject_id = $1")
            .bind("c-1")
            .fetch_one(store.pool())
            .await
            .expect("receipt");
    assert_eq!(basis, "matching unexpired scoped grant");
    // Allowed, so no attention was raised.
    assert_eq!(counts(&store).await.1, 0);

    apply(
        &store,
        "revoke",
        KernelCommand::RevokeAuthority {
            authority_grant_id: AuthorityGrantId::new("g-1"),
            reason: Some("drill complete".into()),
        },
    )
    .await;
    let row =
        sqlx::query("SELECT revoke_reason, revoked_at FROM gwk.authority_grant WHERE id = $1")
            .bind("g-1")
            .fetch_one(store.pool())
            .await
            .expect("grant row");
    assert_eq!(
        row.get::<Option<String>, _>("revoke_reason").as_deref(),
        Some("drill complete")
    );

    // The revoked grant no longer matches, so the same command now pages.
    let (code, _) = refuse(&store, "stop-2", stop("c-2")).await;
    assert_eq!(code, KernelErrorCode::Authority);

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn scope_matches_exactly_and_expiry_is_read_against_the_command() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "scope", 64).await;

    // Scoped to c-1 only. A prefix reading would let this cover c-1-extra.
    apply(&store, "grant", grant("g-1", Some("c-1"), None)).await;
    apply(&store, "allowed", stop("c-1")).await;

    let (code, _) = refuse(&store, "denied", stop("c-1-extra")).await;
    assert_eq!(
        code,
        KernelErrorCode::Authority,
        "an exactly-scoped grant must not reach a longer id"
    );

    // Expiry is compared against the envelope's issued_at (2026-07-28), not the
    // wall clock — otherwise replay would re-decide history.
    apply(
        &store,
        "grant-2",
        grant("g-2", Some("c-9"), Some("2026-07-27T00:00:00Z")),
    )
    .await;
    let (code, _) = refuse(&store, "expired", stop("c-9")).await;
    assert_eq!(
        code,
        KernelErrorCode::Authority,
        "an expired grant is not a grant"
    );

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn an_explicit_attention_item_deduplicates_by_name_and_resolves_once() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "attention", 64).await;

    let raise = |id: &str| KernelCommand::RaiseAttention {
        attention_item_id: AttentionItemId::new(id),
        kind: "risk_tag".to_owned(),
        summary: "data-migration pages".to_owned(),
        subject_ref: Some("task/t-1".to_owned()),
        raised_by: Some(actor("kernel")),
        priority: Some(1),
    };

    apply(&store, "raise", raise("att-1")).await;

    // A second item over the same open (kind, subject_ref) is refused BY NAME —
    // the caller named an id and would otherwise get a silent no-op.
    let (code, message) = refuse(&store, "raise-again", raise("att-2")).await;
    assert_eq!(code, KernelErrorCode::Validation, "{message}");
    assert!(message.contains("att-1"), "{message}");

    apply(
        &store,
        "resolve",
        KernelCommand::ResolveAttention {
            attention_item_id: AttentionItemId::new("att-1"),
            resolution: Some("migration approved".into()),
        },
    )
    .await;
    let resolution: Option<String> =
        sqlx::query_scalar("SELECT resolution FROM gwk.attention_item WHERE id = $1")
            .bind("att-1")
            .fetch_one(store.pool())
            .await
            .expect("resolution");
    assert_eq!(resolution.as_deref(), Some("migration approved"));

    // Resolving twice is a not-found: the UPDATE is predicated on being open.
    let (code, _) = refuse(
        &store,
        "resolve-again",
        KernelCommand::ResolveAttention {
            attention_item_id: AttentionItemId::new("att-1"),
            resolution: None,
        },
    )
    .await;
    assert_eq!(code, KernelErrorCode::NotFound);

    // The dedup slot reopened: the same problem recurring is a NEW item, not a
    // reopened one. That is what `WHERE resolved_at IS NULL` buys.
    apply(&store, "raise-3", raise("att-3")).await;

    drop_database(&maintenance, &name).await;
}

/// Quieting an item is not closing it, and the database is what proves it.
///
/// Ack and mute stamp their own columns and leave `resolved_at` NULL, so the
/// row keeps its `(kind, subject_ref)` dedup slot — which is the mechanism
/// behind "mute is never mapped to resolve". If either verb were implemented as
/// a resolve, the re-raise below would stop being refused and the silenced
/// problem would immediately page again.
#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn acking_and_muting_quiet_an_item_without_resolving_it() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "attentionquiet", 64).await;

    let raise = |id: &str| KernelCommand::RaiseAttention {
        attention_item_id: AttentionItemId::new(id),
        kind: "risk_tag".to_owned(),
        summary: "data-migration pages".to_owned(),
        subject_ref: Some("task/t-1".to_owned()),
        raised_by: Some(actor("kernel")),
        priority: Some(3),
    };
    apply(&store, "raise", raise("att-1")).await;

    let stamps = |store: &PgEventStore| {
        let pool = store.pool().clone();
        async move {
            sqlx::query(
                "SELECT priority, acked_at IS NOT NULL AS acked, \
                        muted_until IS NOT NULL AS muted, \
                        resolved_at IS NOT NULL AS resolved \
                 FROM gwk.attention_item WHERE id = $1",
            )
            .bind("att-1")
            .fetch_one(&pool)
            .await
            .expect("stamps")
        }
    };

    // Raised with a rank: the column has a writer, so it is not a dead column
    // the contract merely promises.
    let row = stamps(&store).await;
    assert_eq!(row.get::<Option<i32>, _>("priority"), Some(3));
    assert!(!row.get::<bool, _>("acked"));

    apply(
        &store,
        "ack",
        KernelCommand::AckAttention {
            attention_item_id: AttentionItemId::new("att-1"),
        },
    )
    .await;
    apply(
        &store,
        "mute",
        KernelCommand::MuteAttention {
            attention_item_id: AttentionItemId::new("att-1"),
            muted_until: Timestamp::new("2026-08-04T00:00:00Z"),
        },
    )
    .await;

    let row = stamps(&store).await;
    assert!(row.get::<bool, _>("acked"), "ack stamps acked_at");
    assert!(row.get::<bool, _>("muted"), "mute stamps muted_until");
    assert!(
        !row.get::<bool, _>("resolved"),
        "neither quieting verb may set resolved_at — that is what makes them \
         distinct from resolve"
    );

    // The dedup slot is STILL HELD. A muted problem does not get to raise
    // itself again the moment it is silenced.
    let (code, message) = refuse(&store, "raise-while-muted", raise("att-2")).await;
    assert_eq!(code, KernelErrorCode::Validation, "{message}");
    assert!(message.contains("att-1"), "{message}");

    // Idempotent last-write-wins: the same key pressed twice is not an error,
    // which is why neither command carries an expected version.
    apply(
        &store,
        "ack-again",
        KernelCommand::AckAttention {
            attention_item_id: AttentionItemId::new("att-1"),
        },
    )
    .await;
    apply(
        &store,
        "mute-again",
        KernelCommand::MuteAttention {
            attention_item_id: AttentionItemId::new("att-1"),
            muted_until: Timestamp::new("2026-08-09T00:00:00Z"),
        },
    )
    .await;
    let muted_until: Option<String> = sqlx::query_scalar(
        "SELECT to_char(muted_until AT TIME ZONE 'UTC', 'YYYY-MM-DD') \
         FROM gwk.attention_item WHERE id = $1",
    )
    .bind("att-1")
    .fetch_one(store.pool())
    .await
    .expect("muted_until");
    assert_eq!(
        muted_until.as_deref(),
        Some("2026-08-09"),
        "the last write wins — re-muting moves the deadline rather than refusing"
    );

    // Naming an item that does not exist is the ONLY way these stamps fail,
    // and it fails typed rather than silently touching nothing.
    let (code, _) = refuse(
        &store,
        "ack-ghost",
        KernelCommand::AckAttention {
            attention_item_id: AttentionItemId::new("att-nope"),
        },
    )
    .await;
    assert_eq!(code, KernelErrorCode::NotFound);

    drop_database(&maintenance, &name).await;
}

/// Unmute clears the mute and touches nothing else.
///
/// The round trip is the point: `muted_until` set, then back to NULL, with
/// `resolved_at` untouched at both ends and the dedup slot held throughout.
/// Mute never resolved the item, so unmute has nothing to re-arm — and if it
/// ever grew a `resolved_at` write, the re-raise below would stop being
/// refused and a silenced problem would come back as a second row.
#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn unmuting_clears_the_stamp_and_re_arms_nothing() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "attentionunmute", 64).await;

    let raise = |id: &str| KernelCommand::RaiseAttention {
        attention_item_id: AttentionItemId::new(id),
        kind: "risk_tag".to_owned(),
        summary: "data-migration pages".to_owned(),
        subject_ref: Some("task/t-1".to_owned()),
        raised_by: Some(actor("kernel")),
        priority: Some(3),
    };
    apply(&store, "raise", raise("att-1")).await;

    let unmute = || KernelCommand::UnmuteAttention {
        attention_item_id: AttentionItemId::new("att-1"),
    };
    let row = |store: &PgEventStore| {
        let pool = store.pool().clone();
        async move {
            sqlx::query(
                "SELECT muted_until IS NOT NULL AS muted, \
                        resolved_at IS NOT NULL AS resolved \
                 FROM gwk.attention_item WHERE id = $1",
            )
            .bind("att-1")
            .fetch_one(&pool)
            .await
            .expect("row")
        }
    };

    apply(
        &store,
        "mute",
        KernelCommand::MuteAttention {
            attention_item_id: AttentionItemId::new("att-1"),
            muted_until: Timestamp::new("2026-08-09T00:00:00Z"),
        },
    )
    .await;
    assert!(row(&store).await.get::<bool, _>("muted"), "mute sets it");

    apply(&store, "unmute", unmute()).await;
    let after = row(&store).await;
    assert!(
        !after.get::<bool, _>("muted"),
        "unmute clears muted_until back to NULL — audible reads the same as \
         never-muted, not as a deadline in the past"
    );
    assert!(
        !after.get::<bool, _>("resolved"),
        "unmute must not touch resolved_at — it is not a resolution any more \
         than mute was"
    );

    // Still held: the item was never resolved, so the same problem still
    // cannot raise a second row.
    let (code, message) = refuse(&store, "raise-after-unmute", raise("att-2")).await;
    assert_eq!(code, KernelErrorCode::Validation, "{message}");
    assert!(message.contains("att-1"), "{message}");

    // Same no-op convention as its siblings: unmuting an already-audible item
    // is a write that changes nothing and succeeds, because the row exists.
    apply(&store, "unmute-again", unmute()).await;
    assert!(!row(&store).await.get::<bool, _>("muted"));

    // And the one failure mode the convention does keep: an id with no row.
    let (code, _) = refuse(
        &store,
        "unmute-ghost",
        KernelCommand::UnmuteAttention {
            attention_item_id: AttentionItemId::new("att-nope"),
        },
    )
    .await;
    assert_eq!(code, KernelErrorCode::NotFound);

    drop_database(&maintenance, &name).await;
}

/// A `subject_ref`-less item never deduplicates — the client obligation, shown.
///
/// The DDL says NULL stays distinct by PostgreSQL default and calls that
/// deliberate, so this is NOT a schema defect to fix; it is the reason
/// client-raised items must always name a subject. Pinning the behaviour here
/// means a future editor who "fixes" the index by making NULLs collide breaks a
/// named test instead of silently swallowing every anonymous page but the first.
#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_null_subject_ref_never_deduplicates() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "attentionnullsubj", 64).await;

    let raise = |id: &str| KernelCommand::RaiseAttention {
        attention_item_id: AttentionItemId::new(id),
        kind: "risk_tag".to_owned(),
        summary: "about nothing in particular".to_owned(),
        subject_ref: None,
        raised_by: Some(actor("kernel")),
        priority: None,
    };

    apply(&store, "raise-1", raise("anon-1")).await;
    // Same kind, both unresolved, no subject: two rows, not a refusal.
    apply(&store, "raise-2", raise("anon-2")).await;
    assert_eq!(
        counts(&store).await.1,
        2,
        "NULL subject_ref does not collide — which is exactly why a client that \
         wants dedup has to pass one"
    );

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn ungated_commands_are_not_evaluated_and_leave_no_receipt() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "ungated", 64).await;

    // Nothing in the work lifecycle is gated, so ordinary work does not need a
    // grant and does not accumulate an audit row per command.
    apply(
        &store,
        "task",
        KernelCommand::CreateTask {
            task_id: gwk_domain::ids::TaskId::new("t-1"),
            kind: None,
            title: None,
            spec_ref: None,
            project: Some(PROJECT.to_owned()),
            priority: None,
            tracker_ref: None,
        },
    )
    .await;
    assert_eq!(counts(&store).await, (0, 0));

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn the_receipt_ledger_refuses_mutation_even_from_the_kernel() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "receiptwrite", 64).await;

    let (code, _) = refuse(&store, "stop", stop("c-1")).await;
    assert_eq!(code, KernelErrorCode::Authority);

    // ENABLE ALWAYS, so this fires for the owner too — the append-only claim is
    // enforced by the database, not by the discipline of the writer.
    for sql in [
        "UPDATE gwk.receipt SET action = 'tampered'",
        "DELETE FROM gwk.receipt",
        // Row triggers never fire on TRUNCATE, so the statement-level partner is
        // what stands between a ledger and one command wiping it.
        "TRUNCATE gwk.receipt",
    ] {
        let refused = sqlx::query(sql).execute(store.pool()).await;
        assert!(refused.is_err(), "{sql} must be refused");
    }

    // A retry derives the same receipt id, so the ledger does not grow a
    // duplicate row per attempt.
    let (code, _) = refuse(&store, "stop", stop("c-1")).await;
    assert_eq!(code, KernelErrorCode::Authority);
    assert_eq!(counts(&store).await.0, 1);

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn the_attention_queue_cannot_be_emptied_only_resolved() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "attentionloss", 64).await;

    // A page the kernel raised itself: no event carries it, so unlike every
    // other projection a replay cannot put this row back. `derived_records` —
    // what a checkpoint and a scratch rebuild hash — leaves this table out for
    // exactly that reason, which is also why nothing downstream would notice.
    let (code, _) = refuse(&store, "stop", stop("c-1")).await;
    assert_eq!(code, KernelErrorCode::Authority);
    assert_eq!(counts(&store).await.1, 1);

    for sql in [
        "DELETE FROM gwk.attention_item",
        "TRUNCATE gwk.attention_item",
    ] {
        let refused = sqlx::query(sql).execute(store.pool()).await;
        assert!(refused.is_err(), "{sql} must be refused");
    }
    assert_eq!(counts(&store).await.1, 1, "the page is still there");

    // And the guard stops at loss: resolving is an UPDATE and still works, or
    // the queue would be immortal instead of durable.
    let resolved = sqlx::query(
        "UPDATE gwk.attention_item SET resolved_at = now(), resolution = 'handled' \
         WHERE resolved_at IS NULL",
    )
    .execute(store.pool())
    .await
    .expect("resolving is not a loss");
    assert_eq!(resolved.rows_affected(), 1);

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_grant_to_another_actor_does_not_allow_this_one() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "grantee", 64).await;

    // The grantee is an Actor value compared whole; a grant issued to the
    // operator must not let the kernel actor through.
    let other = KernelCommand::GrantAuthority {
        authority_grant_id: AuthorityGrantId::new("g-1"),
        grantee: actor("operator"),
        action_class: "stop".to_owned(),
        scope: None,
        expires_at: None,
    };
    let applied = store
        .submit(&envelope_as("grant", actor("kernel"), &other))
        .await;
    assert!(matches!(applied, KernelResult::CommandApplied { .. }));

    let (code, _) = refuse(&store, "stop", stop("c-1")).await;
    assert_eq!(code, KernelErrorCode::Authority);

    drop_database(&maintenance, &name).await;
}
