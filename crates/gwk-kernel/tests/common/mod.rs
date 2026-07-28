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

use gwk_kernel::admin::{self, InitOutcome};
use gwk_kernel::config::{ADMIN_DATABASE_URL_ENV, AdminConfig, RUNTIME_ROLE_ENV};
use gwk_kernel::store::{PgEventStore, connect_pool};
use secrecy::SecretString;
use sqlx::PgPool;

pub const ADMIN_URL_ENV: &str = "GWK_TEST_ADMIN_DATABASE_URL";
pub const RUNTIME_ROLE: &str = "gwk_test_runtime";

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

/// A freshly initialized database plus a store bound to it.
pub async fn fresh_store(
    maintenance: &PgPool,
    tag: &str,
    inflight: usize,
) -> (String, PgEventStore) {
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
