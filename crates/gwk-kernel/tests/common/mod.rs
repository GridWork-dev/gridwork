//! The PostgreSQL harness both integration suites run against.
//!
//! Every case gets its OWN freshly initialized database. The log is append-only
//! by contract, so there is no truncate-and-reuse path, and sharing one would
//! make cases order-dependent — which for an ordering-critical store is exactly
//! the bug the suite is supposed to catch.
//!
//! ```text
//! docker run --rm -d -p 127.0.0.1:55432:5432 -e POSTGRES_HOST_AUTH_METHOD=trust \
//!   --name gwk-pg postgres:16
//! GWK_TEST_ADMIN_DATABASE_URL=postgres://postgres@localhost:55432/postgres \
//!   cargo test -p gwk-kernel -- --ignored
//! ```

// A test-helper module is compiled into EVERY test binary that declares it, so
// the one that uses a subset would otherwise fail `-D warnings` on the rest.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use gwk_domain::blob::{BLOB_CHUNK_BYTES, BlobAddress};
use gwk_domain::command::KernelCommand;
use gwk_domain::envelope::{
    Actor, CommandEnvelope, ENVELOPE_SCHEMA_VERSION, EventEnvelope, Origin, PayloadRef,
};
use gwk_domain::fsm::LeaseMode;
use gwk_domain::ids::{
    AttemptId, AttentionItemId, AuthorityGrantId, ByteCount, CommandId, CostEntryId, CostMicros,
    DispatchNodeId, EngineId, EngineSessionId, EvidenceId, GateId, IdempotencyKey, LeaseId,
    MessageId, ProjectId, Seq, TaskId, Timestamp, TokenCount, WorktreeId,
};
use gwk_domain::ingestion::IngestionKind;
use gwk_domain::inherited::OrchestratorCheckpoint;
use gwk_domain::port::BlobStore;
use gwk_domain::protocol::{
    CONNECTION_EGRESS_BYTES_PER_WINDOW, CONNECTION_INGRESS_BYTES_PER_WINDOW, FRAME_BODY_MAX_BYTES,
    FrameKind, KernelErrorCode, KernelResult, ServerControl,
};
use gwk_kernel::admin::{self, InitOutcome};
use gwk_kernel::blob::store::PgBlobStore;
use gwk_kernel::config::{ADMIN_DATABASE_URL_ENV, AdminConfig, BlobConfig, RUNTIME_ROLE_ENV};
use gwk_kernel::store::{PgEventStore, connect_pool};
use gwk_kernel::wire::frame::{Budget, Incoming, read_frame, write_frame};
use gwk_kernel::wire::listen::Listener;
use gwk_kernel::wire::serve::{Daemon, serve_stream};
use secrecy::SecretString;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::sync::Arc;
use tokio::net::UnixStream;

pub const ADMIN_URL_ENV: &str = "GWK_TEST_ADMIN_DATABASE_URL";
pub const RUNTIME_ROLE: &str = "gwk_test_runtime";

/// The project every case shares unless it is specifically about crossing one.
pub const PROJECT: &str = "p";

/// A syntactically real 40-hex revision. Genesis refuses anything else, and
/// what it records is never resolved against a repository.
pub const TEST_REVISION: &str = "0123456789abcdef0123456789abcdef01234567";
/// The cutover [`fresh_store`] activates at.
pub const TEST_CUTOVER: &str = "cutover-test";
/// A syntactically real archive-manifest digest.
pub const TEST_MANIFEST_SHA256: &str =
    "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0c1d2e3f4a5b6c7d8e9f0a1b2";

pub fn actor(kind: &str) -> Actor {
    Actor {
        kind: kind.to_owned(),
        id: None,
    }
}

/// One command envelope, in a named project. Idempotency is scoped per project
/// while the aggregate namespace is global, so a case that crosses that line
/// has to be able to say which project it is.
pub fn envelope_in(
    project: &str,
    key: &str,
    actor: Actor,
    command: &KernelCommand,
) -> CommandEnvelope {
    CommandEnvelope {
        command_id: CommandId::new(format!("cmd-{project}-{key}")),
        project_id: ProjectId::new(project),
        command_type: command.command_type().to_owned(),
        schema_version: ENVELOPE_SCHEMA_VERSION,
        issued_at: Timestamp::new("2026-07-28T00:00:00Z"),
        actor,
        origin: Origin {
            system: "gw".into(),
            r#ref: None,
        },
        target_aggregate_type: None,
        target_aggregate_id: None,
        expected_version: None,
        idempotency_key: IdempotencyKey::new(key),
        causation_id: None,
        correlation_id: None,
        payload: serde_json::to_value(command).expect("serialize command"),
    }
}

pub fn envelope_as(key: &str, actor: Actor, command: &KernelCommand) -> CommandEnvelope {
    envelope_in(PROJECT, key, actor, command)
}

pub fn envelope(key: &str, command: &KernelCommand) -> CommandEnvelope {
    envelope_as(key, actor("kernel"), command)
}

/// Submit and require success. The key is the caller's, so every case names its
/// own — reusing one is exactly what this kernel is supposed to refuse.
pub async fn apply(store: &PgEventStore, key: &str, command: KernelCommand) -> Vec<EventEnvelope> {
    match store.submit(&envelope(key, &command)).await {
        KernelResult::CommandApplied { events, .. } => events,
        other => panic!("{key}: expected CommandApplied, got {other:?}"),
    }
}

/// Submit and require a refusal, returning the code and message to assert on.
pub async fn refuse(
    store: &PgEventStore,
    key: &str,
    command: KernelCommand,
) -> (KernelErrorCode, String) {
    match store.submit(&envelope(key, &command)).await {
        KernelResult::Error { code, message, .. } => (code, message),
        other => panic!("{key}: expected a refusal, got {other:?}"),
    }
}

/// How many BUSINESS events the log holds.
///
/// The kernel's own aggregate is excluded: genesis and activation are epoch
/// bookkeeping every store carries, and counting them would make each case
/// assert a constant offset that says nothing about the case.
pub async fn event_count(store: &PgEventStore) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM gwk.event WHERE aggregate_type <> 'kernel'")
        .fetch_one(store.pool())
        .await
        .expect("count events")
}

/// Every event, including the epoch's — what the epoch suite asserts on.
pub async fn total_event_count(store: &PgEventStore) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM gwk.event")
        .fetch_one(store.pool())
        .await
        .expect("count events")
}

/// One activation envelope, in the project genesis wrote under. Any other
/// project is refused by the aggregate-ownership rule, which is the point.
pub fn activation(cutover_id: &str) -> CommandEnvelope {
    let command = KernelCommand::ActivateKernel {
        cutover_id: cutover_id.to_owned(),
        archive_manifest_sha256: TEST_MANIFEST_SHA256.to_owned(),
    };
    envelope_in(
        gwk_kernel::SYSTEM_PROJECT,
        &format!("kernel_activated:{cutover_id}"),
        actor("kernel"),
        &command,
    )
}

/// A state row's own view of where it is and what version it carries — the
/// number that must stay equal to its aggregate's version in the log.
pub async fn state_row(store: &PgEventStore, select: &'static str, id: &str) -> (String, i64) {
    use sqlx::Row;
    let row = sqlx::query(select)
        .bind(id)
        .fetch_one(store.pool())
        .await
        .expect("state row");
    (row.get(0), row.get(1))
}

/// The KEK every blob case wraps under. A constant so a case can assert on
/// exact ciphertext behaviour; no deployment would ever hold this value.
pub const TEST_KEK: [u8; 32] = [0x5a; 32];
/// The label recorded beside it, in every container header.
pub const TEST_KEK_ID: &str = "kek-test";

/// A blob store with its OWN root directory, beside `store`'s database.
///
/// Its own root, not a shared one: the store's paths are derived from content
/// digests, so two cases uploading the same bytes would otherwise share a file
/// and one case's sweep would delete the other's blob.
pub async fn blob_store(store: &PgEventStore, tag: &str) -> (PathBuf, PgBlobStore) {
    let root = std::env::temp_dir().join(format!("gwk-blob-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let blobs = blob_store_with(store, &root, TEST_KEK).await;
    (root, blobs)
}

/// An activated store that CAN checkpoint: a blob store on the same database,
/// attached as the home its snapshot records go to.
///
/// The blob store is attached AFTER genesis and activation deliberately. Those
/// two appends run through the same barrier, and a store that checkpointed
/// during its own initialization would snapshot an empty projection set every
/// time — a real answer, but not one any case is asking about.
pub async fn checkpointing_store(
    maintenance: &PgPool,
    tag: &str,
) -> (String, PathBuf, PgEventStore) {
    let (name, store) = fresh_store(maintenance, tag, 8).await;
    let (root, blobs) = blob_store(&store, tag).await;
    (name, root, store.with_blobs(blobs))
}

/// A second store over the SAME root and database under a different KEK — what
/// a deployment looks like after a key rotation.
pub async fn blob_store_with(store: &PgEventStore, root: &Path, kek: [u8; 32]) -> PgBlobStore {
    let config =
        BlobConfig::new(root.to_path_buf(), kek, TEST_KEK_ID.to_owned()).expect("blob config");
    PgBlobStore::open(store.pool().clone(), config)
        .await
        .expect("open blob store")
}

/// Upload `plaintext` in one chunk and commit it at its own address.
pub async fn put(blobs: &PgBlobStore, plaintext: &[u8], media_type: &str) -> BlobAddress {
    let (address, _) = put_dedup(blobs, plaintext, media_type).await;
    address
}

/// The same, keeping the dedup flag the commit reported.
pub async fn put_dedup(
    blobs: &PgBlobStore,
    plaintext: &[u8],
    media_type: &str,
) -> (BlobAddress, bool) {
    let address = address_of(plaintext);
    let upload = blobs
        .begin(
            media_type.to_owned(),
            ByteCount::new(plaintext.len() as u64),
        )
        .await
        .expect("begin");
    // One call per BLOB_CHUNK_BYTES, so a case that wants several container
    // chunks does not also have to drive the wire's chunking by hand.
    for (sequence, chunk) in plaintext.chunks(BLOB_CHUNK_BYTES.max(1)).enumerate() {
        blobs
            .write_chunk(&upload, sequence as u32, chunk)
            .await
            .unwrap_or_else(|e| panic!("chunk {sequence}: {e}"));
    }
    if plaintext.is_empty() {
        blobs
            .write_chunk(&upload, 0, &[])
            .await
            .expect("empty chunk");
    }
    let (descriptor, deduped) = blobs.commit(upload, address.clone()).await.expect("commit");
    assert_eq!(descriptor.address, address);
    (address, deduped)
}

/// What `plaintext` will be addressed as.
pub fn address_of(plaintext: &[u8]) -> BlobAddress {
    let digest: [u8; 32] = Sha256::digest(plaintext).into();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    BlobAddress::from_digest(&hex).expect("digest")
}

/// Read a blob whole, one clamped range at a time.
pub async fn read_all(blobs: &PgBlobStore, address: &BlobAddress, size: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(size as usize);
    while (out.len() as u64) < size {
        let part = blobs
            .read(
                address,
                ByteCount::new(out.len() as u64),
                ByteCount::new(size - out.len() as u64),
            )
            .await
            .expect("read");
        assert!(!part.is_empty(), "read stalled at {}", out.len());
        out.extend_from_slice(&part);
    }
    out
}

pub fn maintenance_url() -> String {
    std::env::var(ADMIN_URL_ENV)
        .unwrap_or_else(|_| panic!("{ADMIN_URL_ENV} must point at a PostgreSQL superuser DSN"))
}

pub fn url_for(database: &str) -> String {
    let base = maintenance_url();
    let (prefix, _) = base.rsplit_once('/').expect("a /database suffix");
    format!("{prefix}/{database}")
}

pub fn secret(database: &str) -> SecretString {
    SecretString::from(url_for(database))
}

/// A freshly initialized database, its genesis appended, and a store bound to
/// it — sealed, so only activation is admitted.
pub async fn fresh_sealed_store(
    maintenance: &PgPool,
    tag: &str,
    inflight: usize,
) -> (String, PgEventStore) {
    let (name, store) = raw_store(maintenance, tag, inflight).await;
    store.ensure_genesis(TEST_REVISION).await.expect("genesis");
    (name, store)
}

/// The same store, activated. Everything that is not about the epoch itself
/// wants this: a sealed kernel admits no business command at all.
pub async fn fresh_store(
    maintenance: &PgPool,
    tag: &str,
    inflight: usize,
) -> (String, PgEventStore) {
    let (name, store) = fresh_sealed_store(maintenance, tag, inflight).await;
    match store.submit(&activation(TEST_CUTOVER)).await {
        KernelResult::CommandApplied { .. } => (name, store),
        other => panic!("activation: expected CommandApplied, got {other:?}"),
    }
}

/// A store on an initialized database with NO genesis — the epoch-less state.
pub async fn raw_store(maintenance: &PgPool, tag: &str, inflight: usize) -> (String, PgEventStore) {
    let name = format!("gwk_store_{}_{tag}", std::process::id());
    drop_database(maintenance, &name).await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name};")))
        .execute(maintenance)
        .await
        .expect("create test database");

    // Two connections per admitted append, the same ratio the daemon binds at:
    // every connection also READS — readiness, a page, a subscription's
    // catch-up — and an append parked waiting for a read to give its connection
    // back would show up as kernel latency that the kernel is not spending.
    let pool = connect_pool(&secret(&name), (inflight as u32 * 2).max(8))
        .await
        .expect("connect");
    let config = AdminConfig::from_lookup({
        let url = url_for(&name);
        move |key| match key {
            ADMIN_DATABASE_URL_ENV => Some(url.clone()),
            RUNTIME_ROLE_ENV => Some(RUNTIME_ROLE.to_owned()),
            _ => None,
        }
    })
    .expect("admin config");
    assert_eq!(
        admin::init(&pool, &config).await.expect("init"),
        InitOutcome::Initialized
    );
    let store = PgEventStore::with_capacity(pool, inflight)
        .await
        .expect("open store");
    (name, store)
}

/// Put `count` synthetic events in the log by INSERT, and return the last
/// sequence written.
///
/// Not through the port, and that is the point: an append is a locked round trip
/// per batch, so the two cases that need a log longer than a page — the read
/// clamp at [`MAX_READ_LIMIT`](gwk_domain::port::MAX_READ_LIMIT) and a batch
/// queue full enough to starve a consumer — would spend minutes in it to prove
/// something about reading. These rows are only ever read.
///
/// `next_seq` is carried forward with them, so a store that appends after a seed
/// allocates past it rather than colliding on the primary key.
///
/// It also sends NO `pg_notify`, which the append path does from inside its own
/// transaction. That makes this the only way to produce the case a subscription's
/// poll exists for: events that are committed and visible while every listener,
/// healthy or not, was never told.
pub async fn seed_events(pool: &PgPool, count: u64) -> Seq {
    seed_with(
        pool,
        count,
        "'evt-seed-' || n, $1, 'task', 'agg-seed-' || n, 1, 'seed_tick', $2, \
         now(), now(), '{\"kind\":\"seed\"}'::jsonb, '{\"system\":\"gwk-test\"}'::jsonb, \
         '{}'::jsonb",
    )
    .await
}

/// The same, with payloads that are real `create_task` command bodies — so the
/// rows can be REPLAYED, which [`seed_events`]'s cannot.
///
/// `padding` bytes of title travel in the event and land in the projection row,
/// exactly as a real create would put them there: this is the "1 KiB inline
/// payload" the envelope's storage bound is about, and dropping it on the
/// projection side would measure a cheaper log than the one being claimed.
pub async fn seed_command_events(pool: &PgPool, count: u64, padding: usize) -> Seq {
    let title = if padding == 0 {
        "null".to_owned()
    } else {
        // A distinct 32 hex characters per row, repeated to length. Nothing here
        // is compressed — the row stays under the TOAST threshold, which is what
        // "inline" means — so the pattern only has to be the right SIZE.
        format!("repeat(md5(n::text), {})", padding.div_ceil(32))
    };
    seed_with(
        pool,
        count,
        &format!(
            "'evt-perf-' || n, $1, 'task', 't-perf-' || n, 1, 'task_created', $2, \
             now(), now(), '{{\"kind\":\"kernel\"}}'::jsonb, \
             '{{\"system\":\"gwk-perf\"}}'::jsonb, \
             jsonb_build_object('type', 'create_task', 'task_id', 't-perf-' || n, \
                'title', {title})"
        ),
    )
    .await
}

/// The projection rows every seeded `task_created` event would have written,
/// derived FROM those events in one statement.
///
/// Every column comes off the event, which is the rule `apply_event` follows —
/// so this writes the same rows a replay would, without the round trip per row.
/// They are born in their INITIAL state at version 1, the only shape
/// `task_born_initial` admits: the same trigger that makes a checkpoint
/// unrestorable also refuses a projection row that no create could have
/// produced.
///
/// For where a row COUNT is what is being measured and paying a replay per row
/// would measure the replay instead. Returns how many rows it wrote.
pub async fn seed_task_rows(pool: &PgPool) -> u64 {
    sqlx::query(
        "INSERT INTO gwk.task (id, version, state, project, title, created_at, updated_at) \
         SELECT e.aggregate_id, e.aggregate_version, 'submitted', e.project_id, \
                e.payload ->> 'title', e.appended_at, e.appended_at \
         FROM gwk.event e \
         WHERE e.aggregate_type = 'task' AND e.event_type = 'task_created' \
         ON CONFLICT (id) DO NOTHING",
    )
    .execute(pool)
    .await
    .expect("seed task rows")
    .rows_affected()
}

/// One `INSERT ... SELECT generate_series` bracketed by the writer's sequence
/// allocation, with the caller supplying every column after `seq`.
async fn seed_with(pool: &PgPool, count: u64, columns: &str) -> Seq {
    let mut tx = pool.begin().await.expect("begin a seed");
    let first: String = sqlx::query_scalar("SELECT next_seq::text FROM gwk_internal.writer")
        .fetch_one(&mut *tx)
        .await
        .expect("read next_seq");
    let first: u64 = first.parse().expect("next_seq is a number");
    let last = first + count - 1;
    // Asserted safe because `columns` comes from this file's own two seeders and
    // never from a row, an argument or an environment variable. Every VALUE is
    // still bound.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "INSERT INTO gwk.event (seq, event_id, project_id, aggregate_type, aggregate_id, \
            aggregate_version, event_type, schema_version, occurred_at, appended_at, \
            actor, origin, payload) \
         SELECT n, {columns} \
         FROM generate_series($3::bigint, $4::bigint) AS n"
    )))
    .bind(PROJECT)
    .bind(i64::from(ENVELOPE_SCHEMA_VERSION))
    .bind(first as i64)
    .bind(last as i64)
    .execute(&mut *tx)
    .await
    .expect("seed events");
    sqlx::query("UPDATE gwk_internal.writer SET next_seq = $1::numeric")
        .bind((last + 1).to_string())
        .execute(&mut *tx)
        .await
        .expect("carry next_seq forward");
    tx.commit().await.expect("commit a seed");
    Seq::new(last)
}

pub async fn drop_database(maintenance: &PgPool, name: &str) {
    let _ = sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {name} WITH (FORCE);"
    )))
    .execute(maintenance)
    .await;
}

pub async fn maintenance_pool() -> PgPool {
    let pool = PgPool::connect(&maintenance_url())
        .await
        .expect("connect to the maintenance database");
    // Result discarded on purpose: cases run concurrently and there is no
    // CREATE ROLE IF NOT EXISTS, so a check-then-create races and the loser
    // gets "already exists" — which is the state it wanted. A role that is
    // genuinely absent still fails loudly, at the GRANT inside `admin::init`.
    let _ = sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE ROLE {RUNTIME_ROLE} NOLOGIN;"
    )))
    .execute(&pool)
    .await;
    pool
}

/// One row in every projection table the kernel can reach today.
///
/// Deliberately exhaustive: `canonical_records` only exercises a table that has
/// rows in it, so an empty table proves nothing about whether its columns still
/// match its contract type. Any projection missing from here is a parity check
/// nobody is running.
pub async fn populate(store: &PgEventStore) {
    apply(
        store,
        "task",
        KernelCommand::CreateTask {
            task_id: TaskId::new("t-1"),
            kind: Some("phase".into()),
            title: Some("ship the kernel".into()),
            spec_ref: None,
            project: Some(PROJECT.to_owned()),
            priority: Some(3),
            tracker_ref: None,
        },
    )
    .await;
    apply(
        store,
        "attempt",
        KernelCommand::CreateAttempt {
            attempt_id: AttemptId::new("a-1"),
            task_id: TaskId::new("t-1"),
            engine: EngineId::new("engine-a"),
            capability: Some("code_write".into()),
            role: None,
            model_lane: Some("standard".into()),
            permission_profile: None,
            worktree_lease_id: None,
            base_sha: None,
            budget: None,
        },
    )
    .await;
    apply(
        store,
        "session",
        KernelCommand::OpenEngineSession {
            engine_session_id: EngineSessionId::new("s-1"),
            attempt_id: AttemptId::new("a-1"),
            engine: EngineId::new("engine-a"),
            provider_session_ref: Some("prov-1".into()),
        },
    )
    .await;
    apply(
        store,
        "node",
        KernelCommand::RegisterDispatchNode {
            dispatch_node_id: DispatchNodeId::new("n-1"),
            parent_id: None,
            attempt_id: Some(AttemptId::new("a-1")),
            kind: "subagent".into(),
            label: Some("reviewer".into()),
        },
    )
    .await;
    apply(
        store,
        "lease",
        KernelCommand::AcquireLease {
            lease_id: LeaseId::new("l-1"),
            mode: LeaseMode::Exclusive,
            holder: Some("a-1".into()),
            scope: Some("worktree".into()),
            repo: Some("gridwork".into()),
            path: Some("/w/kernel".into()),
            branch: Some("feature/kernel".into()),
            base_sha: None,
            expires_at: Some(gwk_domain::ids::Timestamp::new("2026-07-28T01:00:00Z")),
        },
    )
    .await;
    apply(
        store,
        "worktree",
        KernelCommand::RegisterWorktree {
            worktree_id: WorktreeId::new("wt-1"),
            repo: "gridwork".into(),
            path: "/w/kernel".into(),
            branch: "feature/kernel".into(),
            base_sha: None,
            lease_id: Some(LeaseId::new("l-1")),
        },
    )
    .await;
    apply(
        store,
        "message",
        KernelCommand::SendMessage {
            message_id: MessageId::new("m-1"),
            correlation_id: None,
            reply_to: None,
            sender: Some("orchestrator".into()),
            recipient: Some("engine-a".into()),
            channel: Some("dispatch".into()),
            kind: Some("brief".into()),
            payload: Some(serde_json::json!({ "goal": "ship the kernel" })),
            deadline: None,
        },
    )
    .await;
    apply(
        store,
        "gate",
        KernelCommand::OpenGate {
            gate_id: GateId::new("g-1"),
            attempt_id: None,
            phase_ref: Some("4p-kernel".into()),
            kind: Some("review".into()),
        },
    )
    .await;
    // `issue_command` is in the authority risk table, so the grant comes first
    // — and it is the row that populates `authority_grant`.
    apply(
        store,
        "grant",
        KernelCommand::GrantAuthority {
            authority_grant_id: AuthorityGrantId::new("g-stop"),
            grantee: actor("kernel"),
            action_class: "stop".to_owned(),
            scope: None,
            expires_at: None,
        },
    )
    .await;
    // Applying it also writes the receipt every authority decision leaves.
    apply(
        store,
        "command",
        KernelCommand::IssueCommand {
            command_id: CommandId::new("c-1"),
            kind: "stop_attempt".to_owned(),
            targets: vec!["a-1".to_owned()],
            actor: None,
        },
    )
    .await;
    apply(
        store,
        "evidence",
        KernelCommand::RecordEvidence {
            evidence_id: EvidenceId::new("ev-1"),
            kind: "diff".to_owned(),
            r#ref: "blob://sha256-abc".to_owned(),
            digest: Some("sha256-abc".into()),
            // The whole reason the column is `numeric(20,0)` and the canonical
            // form casts it to text: as a JSON number this loses precision
            // above 2^53 and comes back a DIFFERENT value, silently.
            byte_size: Some(ByteCount::new(u64::MAX)),
        },
    )
    .await;
    apply(
        store,
        "attention",
        KernelCommand::RaiseAttention {
            attention_item_id: AttentionItemId::new("att-1"),
            kind: "risk_tag".to_owned(),
            summary: "data-migration pages".to_owned(),
            subject_ref: Some("task/t-1".to_owned()),
            raised_by: Some(actor("kernel")),
        },
    )
    .await;
    apply(
        store,
        "orch",
        KernelCommand::WriteOrchestratorCheckpoint {
            checkpoint: OrchestratorCheckpoint {
                orchestrator_id: Some("orch-1".into()),
                seq: Seq::new(u64::MAX),
                native_session_ref: None,
                active_goal: Some("ship".into()),
                active_step_ref: None,
                latest_command_ref: None,
                open_attempts: Some(vec![]),
                leases: None,
                pending_approvals: None,
                budget_cursor: None,
            },
        },
    )
    .await;
    // The one row whose id nobody supplied: it is `ingest:<project>:<key>`,
    // derived from this very envelope. `u64::MAX` in `event_seq` for the same
    // reason evidence carries it in `byte_size` — a JSON number would come back
    // a different value.
    apply(
        store,
        "ingest",
        KernelCommand::IngestRecord {
            kind: IngestionKind::GraphSnapshot,
            payload: serde_json::json!({ "nodes": 12000, "edges": 48122 }),
            payload_ref: Some(PayloadRef {
                digest: "sha256:abc".to_owned(),
                media_type: "application/json".to_owned(),
                byte_size: ByteCount::new(u64::MAX),
                retention_class: None,
                evidence_pin: None,
            }),
        },
    )
    .await;
    // `u64::MAX` in a token count for the same reason as evidence's
    // `byte_size`: the columns are numeric(20,0) and canonicalize to text.
    apply(
        store,
        "cost",
        KernelCommand::RecordCostEntry {
            cost_entry_id: CostEntryId::new("ce-1"),
            attempt_id: Some(AttemptId::new("a-1")),
            engine_session_id: Some(EngineSessionId::new("s-1")),
            dispatch_node_id: None,
            engine: EngineId::new("engine-a"),
            model: Some("model-x".into()),
            input_tokens: Some(TokenCount::new(u64::MAX)),
            cached_input_tokens: Some(TokenCount::new(800)),
            cache_write_tokens: None,
            output_tokens: Some(TokenCount::new(300)),
            reasoning_tokens: None,
            cost_micros: Some(CostMicros::new(125_000)),
            cost_is_estimate: Some(true),
        },
    )
    .await;
}

pub fn task(id: &str) -> KernelCommand {
    KernelCommand::CreateTask {
        task_id: TaskId::new(id),
        kind: None,
        title: None,
        spec_ref: None,
        project: None,
        priority: None,
        tracker_ref: None,
    }
}

// ---------------------------------------------------------------------------
// The wire half of the harness.
//
// A daemon is a socket, a handshake and a codec before it is a kernel, so every
// suite that wants to ask it something has to speak all three. That client
// lived in `wire.rs` until a second suite needed it: the performance envelope
// measures subscription delivery, which is a question about what arrives at a
// client and cannot be asked any other way.
// ---------------------------------------------------------------------------

/// A private runtime directory, as the daemon requires of its socket's parent.
pub fn runtime_dir(tag: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("gwk-wire-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create runtime dir");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    dir
}

/// A daemon over `store`, with the blob store a serving one is required to have.
///
/// The root comes back so the caller can take it away again: these live under
/// the temp directory, one per case, and a case that left its own behind would
/// hand the next run a directory it did not write.
pub async fn daemon_for(store: PgEventStore, tag: &str) -> (Daemon, PathBuf) {
    let (root, blobs) = blob_store(&store, &format!("wire-{tag}")).await;
    let daemon = Daemon::new(store.with_blobs(blobs), TEST_REVISION.to_owned()).expect("daemon");
    (daemon, root)
}

/// A client that speaks the wire: handshake, then one request per call.
///
/// Generic over its transport, with the socket as the default so every case that
/// wants one says nothing. The slow-consumer case wants a pipe instead: what it
/// has to control is the exact moment the daemon's write blocks, and a kernel
/// socket buffer's capacity is not a number a test can know.
pub struct Client<S = UnixStream> {
    pub stream: S,
    pub budget: Budget,
}

impl Client<UnixStream> {
    pub async fn connect(path: &std::path::Path) -> (Self, ServerControl) {
        Self::greet(UnixStream::connect(path).await.expect("connect")).await
    }
}

impl<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin> Client<S> {
    /// Take an open transport through the handshake.
    pub async fn greet(stream: S) -> (Self, ServerControl) {
        let mut client = Self {
            stream,
            budget: Budget::new(
                CONNECTION_INGRESS_BYTES_PER_WINDOW,
                CONNECTION_EGRESS_BYTES_PER_WINDOW,
            ),
        };
        client
            .send(r#"{"type":"hello","protocol_major":1,"protocol_minor":0,"capabilities":[]}"#)
            .await;
        let ack = client.recv().await.expect("the daemon acked");
        (client, ack)
    }

    pub async fn send(&mut self, raw: &str) {
        write_frame(
            &mut self.stream,
            FrameKind::Json,
            raw.as_bytes(),
            &mut self.budget,
        )
        .await
        .expect("write");
    }

    pub async fn recv(&mut self) -> Option<ServerControl> {
        match read_frame(&mut self.stream, FRAME_BODY_MAX_BYTES, &mut self.budget)
            .await
            .expect("read")
        {
            Incoming::Frame(frame) => {
                Some(serde_json::from_slice(&frame.body).expect("decode the answer"))
            }
            Incoming::Closed => None,
        }
    }

    pub async fn ask(&mut self, id: &str, request: &str) -> KernelResult {
        self.send(&format!(
            r#"{{"type":"request","request_id":"{id}","request":{request}}}"#
        ))
        .await;
        match self.recv().await.expect("a response") {
            ServerControl::Response { request_id, result } => {
                // Every response carries the id it answers: a client with two
                // requests in flight has nothing else to match them by.
                assert_eq!(request_id.as_str(), id);
                result
            }
            other => panic!("{other:?}"),
        }
    }
}

/// A daemon on its own socket with a client already through the handshake.
pub struct Served {
    pub client: Client,
    pub dir: PathBuf,
    pub blobs: PathBuf,
    pub serving: tokio::task::JoinHandle<()>,
}

impl Served {
    pub async fn open(store: PgEventStore, tag: &str) -> Self {
        let dir = runtime_dir(tag);
        let path = dir.join("gwk.sock");
        let listener = Listener::bind(&path).await.expect("bind");
        let (daemon, blobs) = daemon_for(store, tag).await;
        let daemon = Arc::new(daemon);
        let serving = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let _ = serve_stream(&daemon, stream).await;
            listener.remove();
        });
        let (client, _) = Client::connect(&path).await;
        Self {
            client,
            dir,
            blobs,
            serving,
        }
    }

    /// Hang up, let the connection task finish, and take the socket with it.
    pub async fn close(self) {
        drop(self.client);
        self.serving.await.expect("join");
        let _ = std::fs::remove_dir_all(&self.dir);
        let _ = std::fs::remove_dir_all(&self.blobs);
    }
}

/// The real accept loop, serving as many connections as a test opens.
///
/// [`Served`] takes exactly one, which is all request/response needs. A
/// subscription needs two: the client watching the log must not be the client
/// appending to it, or its own submit response and the batch that submit caused
/// arrive on one wire with nothing in the protocol deciding their order. It also
/// gets the notification listener, which `serve_stream` does not start.
pub struct Running {
    pub dir: PathBuf,
    pub path: PathBuf,
    pub blobs: PathBuf,
    pub stop: tokio::sync::oneshot::Sender<()>,
    pub serving: tokio::task::JoinHandle<()>,
}

impl Running {
    pub async fn open(store: PgEventStore, tag: &str) -> Self {
        let dir = runtime_dir(tag);
        let path = dir.join("gwk.sock");
        let listener = Listener::bind(&path).await.expect("bind");
        let (daemon, blobs) = daemon_for(store, tag).await;
        let daemon = Arc::new(daemon);
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let serving = tokio::spawn(async move {
            let _ = gwk_kernel::wire::serve::run(listener, daemon, async move {
                let _ = stopped.await;
            })
            .await;
        });
        Self {
            dir,
            path,
            blobs,
            stop,
            serving,
        }
    }

    pub async fn client(&self) -> Client {
        Client::connect(&self.path).await.0
    }

    /// Stop accepting and drain. Every client must be dropped first — a live one
    /// makes the drain wait out its timeout rather than finish.
    pub async fn close(self) {
        let _ = self.stop.send(());
        self.serving.await.expect("join");
        let _ = std::fs::remove_dir_all(&self.dir);
        let _ = std::fs::remove_dir_all(&self.blobs);
    }
}

/// Subscribe from a known point, so a case that is about later events is not
/// handed the whole log first.
pub fn subscribe_from(cursor: Seq) -> String {
    format!(
        r#"{{"type":"subscribe_events","cursor":"{}"}}"#,
        cursor.value()
    )
}

/// The watermark, as the client sees it.
pub async fn watermark_of(client: &mut Client, id: &str) -> Seq {
    match client.ask(id, r#"{"type":"watermark"}"#).await {
        KernelResult::Watermark { watermark } => watermark.expect("genesis is in the log"),
        other => panic!("{other:?}"),
    }
}
