//! The PostgreSQL harness both integration suites run against.
//!
//! Every case gets its OWN freshly initialized database. The log is append-only
//! by contract, so there is no truncate-and-reuse path, and sharing one would
//! make cases order-dependent — which for an ordering-critical store is exactly
//! the bug the suite is supposed to catch.
//!
//! ```text
//! docker run --rm -d -p 55432:5432 -e POSTGRES_HOST_AUTH_METHOD=trust \
//!   --name gwk-pg postgres:16
//! GWK_TEST_ADMIN_DATABASE_URL=postgres://postgres@localhost:55432/postgres \
//!   cargo test -p gwk-kernel -- --ignored
//! ```

// A test-helper module is compiled into EVERY test binary that declares it, so
// the one that uses a subset would otherwise fail `-D warnings` on the rest.
#![allow(dead_code)]

use gwk_domain::command::KernelCommand;
use gwk_domain::envelope::{
    Actor, CommandEnvelope, ENVELOPE_SCHEMA_VERSION, EventEnvelope, Origin,
};
use gwk_domain::ids::{CommandId, IdempotencyKey, ProjectId, Timestamp};
use gwk_domain::protocol::{KernelErrorCode, KernelResult};
use gwk_kernel::admin::{self, InitOutcome};
use gwk_kernel::config::{ADMIN_DATABASE_URL_ENV, AdminConfig, RUNTIME_ROLE_ENV};
use gwk_kernel::store::{PgEventStore, connect_pool};
use secrecy::SecretString;
use sqlx::PgPool;

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

    let pool = connect_pool(&secret(&name), 8).await.expect("connect");
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
