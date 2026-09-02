//! Certifies the Context lifecycle append path and its truth projection: that
//! the four producing facts land their rows, that every cardinality the DDL
//! enforces is a typed refusal before the database says it, that observation
//! order is enforced where nothing else enforces it, that a retry is idempotent,
//! and that a replay rebuilds the same rows.
//!
//! What only a real database can show: that the DDL and the Rust newtypes agree
//! on a maximal legal record — the id charset, nine `sha256:` CHECKs, five count
//! bounds, both validator functions and the assurance token are closed in Rust
//! and re-checked in SQL, and only an INSERT proves the two closures are the
//! same one. And that a refusal is a refusal: a pre-check whose absence would be
//! covered by a unique index looks identical from the caller's side unless the
//! error CODE is asserted.
//!
//! `#[ignore]` because it needs a server — see `tests/common/mod.rs`.

mod common;

use common::{PROJECT, apply, drop_database, fresh_store, maintenance_pool, raw_store, task};
use gwk_domain::command::KernelCommand;
use gwk_domain::ids::{
    AttemptId, ByteCount, EngineId, EvidenceId, IdempotencyKey, ProjectId, TaskId, Timestamp,
};
use gwk_domain::protocol::KernelErrorCode;
use gwk_domain::{
    Assurance, AttributionPart, ContentClass, ContextAttribution, ContextFact, ContextRunId,
    Digest, EvidenceRefs, FinalizationSupplementId, ManifestId, ObservationIndex,
    ObservationSupplementId, Participation, ParticipationReason, ParticipationRecords,
    RecordContextFact, RecordCount, ReleaseSupplementId, VerificationVerdict,
};
use gwk_kernel::PgEventStore;
use gwk_kernel::project::Refusal;
use gwk_kernel::recover::Verdict;

const HEX_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HEX_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn digest(hex: &str) -> Digest {
    Digest::from_hex(hex).expect("a valid digest")
}

fn count(value: u32) -> RecordCount {
    RecordCount::new(value).expect("a bounded count")
}

fn at(minute: u32) -> Timestamp {
    Timestamp::new(format!("2026-08-14T12:{minute:02}:00Z"))
}

fn manifest_id() -> ManifestId {
    ManifestId::parse("manifest-1").expect("a valid id")
}

fn run_id() -> ContextRunId {
    ContextRunId::parse("run-1").expect("a valid id")
}

fn release_id() -> ReleaseSupplementId {
    ReleaseSupplementId::parse("release-1").expect("a valid id")
}

fn evidence() -> EvidenceRefs {
    EvidenceRefs::new(vec![EvidenceId::new("evidence-1")]).expect("bounded evidence")
}

/// Compiler-derived provenance. The kernel never mints one, so every case
/// supplies it the way the compiler would.
fn attribution() -> ContextAttribution {
    ContextAttribution {
        compiler: AttributionPart::parse("gwk-context-compiler/0.0.1").expect("a valid component"),
        route_digest: digest(HEX_A),
        authority_digest: digest(HEX_A),
        derived_from: manifest_id(),
    }
}

/// Record one fact and require success.
async fn record(store: &PgEventStore, key: &str, fact: ContextFact) {
    record_result(store, key, fact)
        .await
        .unwrap_or_else(|refusal| panic!("{key}: expected the fact to land, got {refusal}"));
}

async fn record_result(
    store: &PgEventStore,
    key: &str,
    fact: ContextFact,
) -> Result<gwk_kernel::context::ContextAppended, Refusal> {
    store
        .record_context_fact(
            &ProjectId::new(PROJECT),
            &IdempotencyKey::new(key),
            &attribution(),
            &RecordContextFact { fact },
        )
        .await
}

/// A task and the attempt `gwk.context_manifest.attempt_id` points at. The FK
/// is real, so the row cannot exist without them.
async fn seed_attempt(store: &PgEventStore) {
    apply(store, "ctx-task", task("t-1")).await;
    apply(
        store,
        "ctx-attempt",
        KernelCommand::CreateAttempt {
            attempt_id: AttemptId::new("a-1"),
            task_id: TaskId::new("t-1"),
            engine: EngineId::new("engine-a"),
            capability: None,
            role: None,
            model_lane: None,
            permission_profile: None,
            worktree_lease_id: None,
            base_sha: None,
            budget: None,
        },
    )
    .await;
}

fn compilation_requested() -> ContextFact {
    ContextFact::CompilationRequested {
        attempt_id: AttemptId::new("a-1"),
        route_digest: digest(HEX_A),
        authority_digest: digest(HEX_A),
        requested_at: at(0),
    }
}

/// A manifest whose participation set the caller chooses.
///
/// `manifest_resolved` keeps the empty set every other case wants; the class
/// cases need rows, and rows are the only thing they vary.
fn manifest_resolved_with(participations: ParticipationRecords) -> ContextFact {
    match manifest_resolved() {
        ContextFact::ManifestResolved {
            manifest_id,
            attempt_id,
            manifest_digest,
            route_digest,
            authority_digest,
            source_count,
            source_bytes,
            evidence_ids,
            resolved_at,
            ..
        } => ContextFact::ManifestResolved {
            manifest_id,
            attempt_id,
            manifest_digest,
            route_digest,
            authority_digest,
            source_count,
            source_bytes,
            participations,
            evidence_ids,
            resolved_at,
        },
        other => panic!("manifest_resolved stopped being a ManifestResolved: {other:?}"),
    }
}

/// Classify one digest in the Context CAS, the way a sealing put would.
async fn seal(store: &PgEventStore, digest: &Digest, class: ContentClass) {
    sqlx::query(
        "INSERT INTO gwk.context_blob (digest, content_class, redaction_class, retention_class) \
         VALUES ($1, $2, 'none', 'manifest')",
    )
    .bind(digest.as_str())
    .bind(class.as_str())
    .execute(store.pool())
    .await
    .expect("the classification row lands");
}

fn manifest_resolved() -> ContextFact {
    ContextFact::ManifestResolved {
        manifest_id: manifest_id(),
        attempt_id: AttemptId::new("a-1"),
        manifest_digest: digest(HEX_B),
        route_digest: digest(HEX_A),
        authority_digest: digest(HEX_A),
        source_count: count(2),
        source_bytes: ByteCount::new(4096),
        participations: ParticipationRecords::new(Vec::new()).expect("empty is valid"),
        evidence_ids: evidence(),
        resolved_at: at(1),
    }
}

fn verification(verdict: VerificationVerdict) -> ContextFact {
    ContextFact::ManifestVerificationRecorded {
        manifest_id: manifest_id(),
        manifest_digest: digest(HEX_B),
        verdict,
        verification_digest: digest(HEX_A),
        evidence_ids: evidence(),
        verified_at: at(2),
    }
}

fn release_recorded() -> ContextFact {
    ContextFact::ReleaseRecorded {
        manifest_id: manifest_id(),
        release_id: release_id(),
        rendered_digest: digest(HEX_A),
        tool_schema_digest: digest(HEX_B),
        rendered_bytes: ByteCount::new(2048),
        tool_schema_count: count(7),
        evidence_ids: evidence(),
        released_at: at(3),
    }
}

fn run_opened() -> ContextFact {
    ContextFact::RunOpened {
        run_id: run_id(),
        manifest_id: manifest_id(),
        release_id: release_id(),
        opened_at: at(4),
    }
}

fn observation(index: u32) -> ContextFact {
    ContextFact::ObservationAppended {
        run_id: run_id(),
        manifest_id: manifest_id(),
        observation_id: ObservationSupplementId::parse(&format!("observation-{index}"))
            .expect("a valid id"),
        observation_index: ObservationIndex::new(index).expect("a valid index"),
        fact_digest: digest(HEX_A),
        observed_bytes: ByteCount::new(512),
        visible_source_count: count(2),
        truncated: false,
        evidence_ids: evidence(),
        observed_at: at(10 + index),
    }
}

fn run_closed() -> ContextFact {
    ContextFact::RunClosed {
        run_id: run_id(),
        finalization_id: FinalizationSupplementId::parse("final-1").expect("a valid id"),
        output_digest: digest(HEX_B),
        observation_count: count(3),
        lifecycle_complete: true,
        closed_at: at(20),
    }
}

fn assurance_certified() -> ContextFact {
    ContextFact::AssuranceCertified {
        run_id: run_id(),
        manifest_id: manifest_id(),
        finalization_id: FinalizationSupplementId::parse("final-1").expect("a valid id"),
        output_digest: digest(HEX_B),
        verification_digest: digest(HEX_A),
        approval_count: count(1),
        observation_count: count(3),
        final_event_root: digest(HEX_B),
        lifecycle_complete: true,
        assurance: Assurance::Trace,
        evidence_ids: evidence(),
        closed_at: at(20),
        certified_at: at(30),
    }
}

/// The whole lifecycle in order, through the verification the certification
/// needs. Every case that wants rows to exist starts here.
async fn lifecycle(store: &PgEventStore) {
    seed_attempt(store).await;
    record(store, "ctx-1", compilation_requested()).await;
    record(store, "ctx-2", manifest_resolved()).await;
    record(store, "ctx-3", verification(VerificationVerdict::Verified)).await;
    record(store, "ctx-4", release_recorded()).await;
    record(store, "ctx-5", run_opened()).await;
    for index in 1..=3 {
        record(store, &format!("ctx-obs-{index}"), observation(index)).await;
    }
    record(store, "ctx-6", run_closed()).await;
    record(store, "ctx-7", assurance_certified()).await;
}

async fn count_rows(store: &PgEventStore, table: &str) -> i64 {
    count_rows_in(store.pool(), table).await
}

async fn count_rows_in(pool: &sqlx::PgPool, table: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
        .fetch_one(pool)
        .await
        .unwrap_or_else(|e| panic!("count {table}: {e}"))
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn the_four_truth_records_land_from_their_facts() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "ctxlife", 8).await;
    lifecycle(&store).await;

    // The counts first. Everything below reads a column out of a row, and a
    // read from a table with no rows in it proves nothing about the column.
    assert_eq!(count_rows(&store, "gwk.context_manifest").await, 1);
    assert_eq!(count_rows(&store, "gwk.context_release").await, 1);
    assert_eq!(count_rows(&store, "gwk.context_observation").await, 3);
    assert_eq!(count_rows(&store, "gwk.context_finalization").await, 1);

    // The columns no fact used to carry, which is the whole reason the grammar
    // widened. A zero here would pass every CHECK the schema has.
    let (schemas, bytes, visible): (i32, String, i32) = sqlx::query_as(
        "SELECT r.tool_schema_count, o.observed_bytes::text, o.visible_source_count \
         FROM gwk.context_release r \
         JOIN gwk.context_observation o ON o.manifest_id = r.manifest_id \
         WHERE o.observation_index = 1",
    )
    .fetch_one(store.pool())
    .await
    .expect("the release and its first observation");
    assert_eq!(schemas, 7, "tool_schema_count must come from the fact");
    assert_eq!(bytes, "512", "observed_bytes must come from the fact");
    assert_eq!(visible, 2, "visible_source_count must come from the fact");

    let evidence: Vec<String> =
        sqlx::query_scalar("SELECT evidence_ids FROM gwk.context_manifest WHERE id = $1")
            .bind(manifest_id().as_str())
            .fetch_one(store.pool())
            .await
            .expect("the manifest row");
    assert_eq!(evidence, vec!["evidence-1".to_owned()]);

    // Observation order, as a set rather than a count: three rows at indices
    // 1, 2, 3 and a count of three are not the same claim, and a count cannot
    // see a substitution.
    let indices: Vec<i32> = sqlx::query_scalar(
        "SELECT observation_index FROM gwk.context_observation ORDER BY observation_index",
    )
    .fetch_all(store.pool())
    .await
    .expect("the observations");
    assert_eq!(indices, vec![1, 2, 3]);

    // The finalization row spans two facts by design, so both halves are
    // checked: the assurance token that only the certification carries, and
    // the timestamp that is deliberately the close time rather than the
    // certification time.
    let (assurance, verification, finalized): (String, String, String) = sqlx::query_as(
        "SELECT assurance, verification_digest, to_char(finalized_at, 'HH24:MI') \
         FROM gwk.context_finalization",
    )
    .fetch_one(store.pool())
    .await
    .expect("the finalization row");
    assert_eq!(assurance, "trace");
    assert_eq!(verification, format!("sha256:{HEX_A}"));
    assert_eq!(
        finalized, "12:20",
        "finalized_at is the run's close time, not the certification's"
    );

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_duplicate_release_is_a_typed_refusal_not_a_constraint_violation() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "ctxduprel", 8).await;
    seed_attempt(&store).await;
    record(&store, "ctx-1", manifest_resolved()).await;
    record(&store, "ctx-2", release_recorded()).await;

    // A second release for the same manifest, under a different id and a
    // different key, so nothing but the one-per-manifest rule can refuse it.
    let second = ContextFact::ReleaseRecorded {
        manifest_id: manifest_id(),
        release_id: ReleaseSupplementId::parse("release-2").expect("a valid id"),
        rendered_digest: digest(HEX_A),
        tool_schema_digest: digest(HEX_B),
        rendered_bytes: ByteCount::new(2048),
        tool_schema_count: count(7),
        evidence_ids: evidence(),
        released_at: at(4),
    };
    let refusal = record_result(&store, "ctx-3", second)
        .await
        .expect_err("a second release for one manifest must be refused");

    // The CODE, not merely that it errored. Deleting the pre-check leaves the
    // DDL refusing underneath, and a raw unique violation arrives as Storage —
    // which is exactly what makes the code the discriminating assertion.
    assert_eq!(refusal.code, KernelErrorCode::IdempotencyConflict);
    assert!(
        refusal.message.contains(manifest_id().as_str()),
        "the refusal must name the manifest: {}",
        refusal.message
    );
    // A refusal that also wrote is not a refusal.
    assert_eq!(count_rows(&store, "gwk.context_release").await, 1);

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn an_out_of_order_observation_index_is_refused() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "ctxorder", 8).await;
    seed_attempt(&store).await;
    record(&store, "ctx-1", manifest_resolved()).await;
    record(&store, "ctx-2", release_recorded()).await;
    record(&store, "ctx-3", run_opened()).await;
    record(&store, "ctx-4", observation(1)).await;

    // Index 3 after index 1. The schema bounds this column and pins its
    // uniqueness and would take this row without complaint; nothing but the
    // handler notices the gap.
    let refusal = record_result(&store, "ctx-5", observation(3))
        .await
        .expect_err("a gap in the observation sequence must be refused");
    assert_eq!(refusal.code, KernelErrorCode::Validation);
    assert!(
        refusal.message.contains("expects observation index 2"),
        "the refusal must name the index it wanted: {}",
        refusal.message
    );
    assert_eq!(count_rows(&store, "gwk.context_observation").await, 1);

    // The positive control: index 2 is accepted, so the assertion above is
    // about the gap and not about observations being unwritable at all.
    record(&store, "ctx-6", observation(2)).await;
    assert_eq!(count_rows(&store, "gwk.context_observation").await, 2);

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_rejected_verification_cannot_be_certified() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "ctxreject", 8).await;
    seed_attempt(&store).await;
    record(&store, "ctx-1", manifest_resolved()).await;
    record(&store, "ctx-2", verification(VerificationVerdict::Rejected)).await;
    record(&store, "ctx-3", release_recorded()).await;
    record(&store, "ctx-4", run_opened()).await;
    record(&store, "ctx-5", run_closed()).await;

    // `gwk.context_finalization` records a verification_digest and has no
    // column for the verdict behind it, so a rejected verification would land
    // in a row that reads exactly like an accepted one. Both digests are
    // well-formed sha-256, so no CHECK can tell them apart.
    let refusal = record_result(&store, "ctx-6", assurance_certified())
        .await
        .expect_err("a rejected manifest must not be certifiable");
    assert_eq!(refusal.code, KernelErrorCode::Validation);
    assert!(
        refusal.message.contains("rejected by its verifier"),
        "the refusal must say why: {}",
        refusal.message
    );
    assert_eq!(count_rows(&store, "gwk.context_finalization").await, 0);

    // The digest cross-check is the other half of the same read, and it fires
    // on a claim no verifier answered for even when the verdict was clean.
    record(&store, "ctx-7", verification(VerificationVerdict::Verified)).await;
    let mistaken = match assurance_certified() {
        ContextFact::AssuranceCertified {
            run_id,
            manifest_id,
            finalization_id,
            output_digest,
            approval_count,
            observation_count,
            final_event_root,
            lifecycle_complete,
            assurance,
            evidence_ids,
            closed_at,
            certified_at,
            ..
        } => ContextFact::AssuranceCertified {
            run_id,
            manifest_id,
            finalization_id,
            output_digest,
            verification_digest: digest(HEX_B),
            approval_count,
            observation_count,
            final_event_root,
            lifecycle_complete,
            assurance,
            evidence_ids,
            closed_at,
            certified_at,
        },
        _ => unreachable!(),
    };
    let refusal = record_result(&store, "ctx-8", mistaken)
        .await
        .expect_err("a verification digest no verifier answered for must be refused");
    assert_eq!(refusal.code, KernelErrorCode::Validation);
    assert!(
        refusal.message.contains("was verified under"),
        "the refusal must name both digests: {}",
        refusal.message
    );

    // And the positive control, so neither assertion above passes because
    // certification is simply impossible: the honest fact lands.
    record(&store, "ctx-9", assurance_certified()).await;
    assert_eq!(count_rows(&store, "gwk.context_finalization").await, 1);

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_retried_identical_fact_is_idempotent() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "ctxidem", 8).await;
    seed_attempt(&store).await;
    let first = record_result(&store, "ctx-1", manifest_resolved())
        .await
        .expect("the first append");
    assert!(!first.replayed);
    assert_eq!(count_rows(&store, "gwk.context_manifest").await, 1);

    // The same fact under the same key. The one-per-attempt pre-check would
    // refuse it if the key check did not answer first, so this also pins the
    // order those two run in.
    let second = record_result(&store, "ctx-1", manifest_resolved())
        .await
        .expect("the retry must be answered, not refused");
    assert!(second.replayed, "a retried key must answer from the log");
    assert_eq!(
        second
            .events
            .first()
            .map(|e| e.event_id.as_str().to_owned()),
        first.events.first().map(|e| e.event_id.as_str().to_owned()),
        "a retry must return the original event"
    );
    assert_eq!(count_rows(&store, "gwk.context_manifest").await, 1);

    // A different fact under the same key is a conflict, not a replay.
    let refusal = record_result(&store, "ctx-1", compilation_requested())
        .await
        .expect_err("a reused key naming a different request must be refused");
    assert_eq!(refusal.code, KernelErrorCode::IdempotencyConflict);

    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_replay_rebuilds_the_same_context_rows() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "ctxreplay", 8).await;
    lifecycle(&store).await;

    // Reported before anything is asserted about projections, so a red test
    // says whether the EVENTS were seeded before anyone goes looking at rows.
    let events = common::event_count(&store).await;
    assert!(
        events >= 10,
        "expected the lifecycle's events, got {events}"
    );

    let (scratch_name, scratch) = raw_store(&maintenance, "ctxreplayscratch", 8).await;
    let report = store
        .rebuild_into(scratch.pool())
        .await
        .expect("rebuild into scratch");
    assert!(report.agrees, "a healthy log must rebuild to what it built");

    // Against the literal 3 this test itself submitted, on BOTH databases, as
    // two separate assertions. Asserting scratch == live instead would let a
    // skipped arm in the shared applier remove the rows from both sides and
    // stay green — the expectation would move with the mutation.
    assert_eq!(count_rows(&store, "gwk.context_observation").await, 3);
    assert_eq!(
        count_rows_in(scratch.pool(), "gwk.context_observation").await,
        3
    );
    assert_eq!(
        count_rows_in(scratch.pool(), "gwk.context_manifest").await,
        1
    );
    assert_eq!(
        count_rows_in(scratch.pool(), "gwk.context_release").await,
        1
    );
    assert_eq!(
        count_rows_in(scratch.pool(), "gwk.context_finalization").await,
        1
    );

    // The replay-over-existing-rows case is NOT exercised here, and that is a
    // statement about the system rather than a gap in the suite: it cannot be
    // reached. `recover` replays into live only when the declared projections
    // are empty, `gwk.context_manifest.attempt_id` references `gwk.attempt`
    // under plain NO ACTION, and `attempt` is the first declared projection —
    // so a Context row existing implies the projections are not empty. See the
    // module docs on `gwk_kernel::context`.
    let recovered = store.recover().await.expect("recover");
    assert!(
        recovered.ready(),
        "the store must be servable after a replay"
    );
    // `Unverified` specifically, not merely `ready()`: `ready()` is also true
    // for Verified and Replayed, so it cannot say which path ran.
    assert!(
        matches!(recovered.verdict, Verdict::Unverified { .. }),
        "expected Unverified after appends past the newest checkpoint, got {:?}",
        recovered.verdict
    );

    drop_database(&maintenance, &scratch_name).await;
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_participation_cannot_claim_a_class_the_cas_contradicts() {
    // The invariant the operator ruled on 2026-09-02: a participation states
    // its own class, and where the CAS already holds those bytes the two have
    // to agree. Two recordings of one seam is the price of classifying
    // candidates that were never sealed; this is what stops them diverging.
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "ctxclass", 8).await;
    seed_attempt(&store).await;
    record(&store, "ctx-request", compilation_requested()).await;

    let sealed = digest(HEX_A);
    let never_sealed = digest(HEX_B);
    seal(&store, &sealed, ContentClass::Private).await;

    // The row exists and says `private` — asserted before anything is deduced
    // from a refusal, because a guard that refused for the wrong reason would
    // look identical from the outside.
    let stored: String =
        sqlx::query_scalar("SELECT content_class FROM gwk.context_blob WHERE digest = $1")
            .bind(sealed.as_str())
            .fetch_one(store.pool())
            .await
            .expect("the classification row");
    assert_eq!(
        stored, "private",
        "the fixture did not classify what it meant to"
    );

    let disagreeing = ParticipationRecords::new(vec![Participation::active(
        sealed.clone(),
        ContentClass::Conformance,
    )])
    .expect("a legal record");
    let refusal = record_result(
        &store,
        "ctx-class-disagree",
        manifest_resolved_with(disagreeing),
    )
    .await
    .expect_err("a class the CAS contradicts must be refused");
    assert_eq!(refusal.code, KernelErrorCode::Validation);
    // The message names the digest and both sides, because an operator reading
    // it has to know WHICH candidate and which way round.
    assert!(
        refusal.message.contains(sealed.as_str()),
        "the refusal does not name the candidate: {}",
        refusal.message
    );
    assert!(
        refusal.message.contains("conformance") && refusal.message.contains("private"),
        "the refusal does not name both classes: {}",
        refusal.message
    );

    // Nothing landed. A refusal that still wrote the row would pass every
    // assertion above.
    assert_eq!(count_rows(&store, "gwk.context_manifest").await, 0);

    // POSITIVE CONTROL, and it carries the whole point of the design: the
    // agreeing candidate is admitted, and so is one the CAS has never seen.
    // Without this arm, a guard that refused every manifest with any
    // participation at all would pass the refusal case above.
    let agreeing = ParticipationRecords::new(vec![
        Participation::active(sealed.clone(), ContentClass::Private),
        Participation::excluded(
            never_sealed.clone(),
            ContentClass::Conformance,
            ParticipationReason::BudgetCut,
        ),
    ])
    .expect("a legal record");
    record(&store, "ctx-class-agree", manifest_resolved_with(agreeing)).await;
    assert_eq!(count_rows(&store, "gwk.context_manifest").await, 1);

    // And the class survived into the row rather than being dropped on the
    // way through the applier: read it back per candidate, by name.
    let classes: Vec<(String, String)> = sqlx::query_as(
        "SELECT p ->> 'digest', p ->> 'class' \
         FROM gwk.context_manifest m, jsonb_array_elements(m.participations) AS p \
         ORDER BY 1",
    )
    .fetch_all(store.pool())
    .await
    .expect("the participation rows");
    assert_eq!(
        classes,
        vec![
            (sealed.as_str().to_owned(), "private".to_owned()),
            (never_sealed.as_str().to_owned(), "conformance".to_owned()),
        ],
        "the stored classes are not the ones the fact stated"
    );

    drop_database(&maintenance, &name).await;
}
