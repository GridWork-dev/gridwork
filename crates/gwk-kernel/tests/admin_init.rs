//! Proves initialization against a real PostgreSQL.
//!
//! The unit tests beside `admin.rs` cover the decision table; they cannot cover
//! whether the DDL applies, whether the grants land, or whether the refusals
//! fire against a server. This does, and it is `#[ignore]` because it needs one:
//!
//! ```text
//! docker run --rm -d -p 127.0.0.1:55432:5432 -e POSTGRES_HOST_AUTH_METHOD=trust \
//!   --name gwk-pg postgres:16
//! GWK_TEST_ADMIN_DATABASE_URL=postgres://postgres@localhost:55432/postgres \
//!   cargo test -p gwk-kernel --test admin_init -- --ignored
//! ```
//!
//! Trust auth, so the throwaway container needs no password at all — and so
//! this file carries no credential-shaped literal for the leak gate to find.
//! That is only safe because the publish is bound to loopback: `-p 55432:5432`
//! would put a passwordless superuser on every interface the host has.
//!
//! Wiring it into CI beside the existing `schema` job belongs to the task that
//! gates the kernel; this is the local proof until then.

use gwk_kernel::admin::{self, InitOutcome, TargetState};
use gwk_kernel::config::{ADMIN_DATABASE_URL_ENV, AdminConfig, RUNTIME_ROLE_ENV};
use gwk_kernel::contract_sql::CONTRACT_SQL_SHA256;
use sqlx::{Connection, PgConnection, PgPool, Row};

const ADMIN_URL_ENV: &str = "GWK_TEST_ADMIN_DATABASE_URL";
const RUNTIME_ROLE: &str = "gwk_test_runtime";

fn maintenance_url() -> String {
    std::env::var(ADMIN_URL_ENV).unwrap_or_else(|_| {
        panic!("{ADMIN_URL_ENV} must point at a PostgreSQL superuser DSN for this test")
    })
}

/// A DSN for `database` on the same server as the maintenance DSN.
fn url_for(database: &str) -> String {
    let base = maintenance_url();
    let (prefix, _) = base
        .rsplit_once('/')
        .expect("a postgres URL carries a /database suffix");
    format!("{prefix}/{database}")
}

fn admin_config(database: &str) -> AdminConfig {
    let url = url_for(database);
    AdminConfig::from_lookup(move |key| match key {
        ADMIN_DATABASE_URL_ENV => Some(url.clone()),
        RUNTIME_ROLE_ENV => Some(RUNTIME_ROLE.to_owned()),
        _ => None,
    })
    .expect("test config")
}

/// A uniquely named, freshly created, empty database. Dropped by the caller.
async fn fresh_database(maintenance: &PgPool, suffix: &str) -> String {
    let name = format!("gwk_init_{}_{suffix}", std::process::id());
    // One statement per call: a multi-statement simple query runs as one
    // implicit transaction, and DROP/CREATE DATABASE cannot. Identifiers here
    // are built from a pid and a literal, never from input.
    drop_database(maintenance, &name).await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name};")))
        .execute(maintenance)
        .await
        .expect("create test database");
    name
}

async fn drop_database(maintenance: &PgPool, name: &str) {
    let _ = sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {name};"
    )))
    .execute(maintenance)
    .await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn init_applies_the_contract_is_idempotent_and_refuses_a_stranger() {
    let maintenance = PgPool::connect(&maintenance_url())
        .await
        .expect("connect to the maintenance database");

    // The runtime role is created by the operator, never by init.
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)")
            .bind(RUNTIME_ROLE)
            .fetch_one(&maintenance)
            .await
            .expect("query pg_roles");
    if !exists {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "CREATE ROLE {RUNTIME_ROLE} NOLOGIN;"
        )))
        .execute(&maintenance)
        .await
        .expect("create the runtime role");
    }

    let fresh = fresh_database(&maintenance, "fresh").await;
    let occupied = fresh_database(&maintenance, "occupied").await;

    {
        let pool = PgPool::connect(&url_for(&fresh)).await.expect("connect");
        assert_eq!(
            admin::inspect(&pool).await.expect("inspect"),
            TargetState::Empty
        );

        let config = admin_config(&fresh);
        assert_eq!(
            admin::init(&pool, &config).await.expect("init"),
            InitOutcome::Initialized
        );
        // The contract really applied: the transition seed is queryable.
        let edges: i64 = sqlx::query_scalar("SELECT count(*) FROM gwk.transition")
            .fetch_one(&pool)
            .await
            .expect("count seeded transitions");
        assert!(edges > 0, "the contract's transition seed did not apply");

        assert_eq!(
            admin::inspect(&pool).await.expect("inspect"),
            TargetState::Initialized {
                contract_sha256: CONTRACT_SQL_SHA256.to_owned()
            }
        );
        // A retried operator command is a no-op, not a second contract.
        assert_eq!(
            admin::init(&pool, &config).await.expect("re-init"),
            InitOutcome::AlreadyInitialized
        );

        // The granted role holds nothing the kernel refuses to run with, and
        // cannot rewrite history even though it can append to it.
        let mut conn = PgConnection::connect(&url_for(&fresh))
            .await
            .expect("dedicated connection");
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("SET ROLE {RUNTIME_ROLE};")))
            .execute(&mut conn)
            .await
            .expect("assume the runtime role");
        let privileges = admin::runtime_privileges(&mut conn)
            .await
            .expect("runtime privileges");
        assert!(
            privileges.violations().is_empty(),
            "granted role holds: {:?}",
            privileges.violations()
        );
        // The whole grant matrix, not a spot check: a REVOKE that names the
        // wrong table leaves one relation writable, and only an exhaustive
        // sweep of the schema notices. `write_tables` is every table the
        // kernel rebuilds; the rest are the log, the audit trail, and the
        // contract's own FSM seed.
        let write_tables = [
            "attempt",
            "attention_item",
            "authority_grant",
            "command",
            "dispatch_node",
            "engine_session",
            "evidence",
            "gate",
            "lease",
            "message",
            "orchestrator_checkpoint",
            "task",
            "workflow_run",
            "worktree",
        ];
        let rows = sqlx::query(
            "SELECT c.relname, \
               has_table_privilege(c.oid, 'SELECT')   AS sel, \
               has_table_privilege(c.oid, 'INSERT')   AS ins, \
               has_table_privilege(c.oid, 'UPDATE')   AS upd, \
               has_table_privilege(c.oid, 'DELETE')   AS del, \
               has_table_privilege(c.oid, 'TRUNCATE') AS trunc \
             FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE n.nspname = 'gwk' AND c.relkind IN ('r', 'p', 'S', 'v', 'm') \
             ORDER BY 1",
        )
        .fetch_all(&mut conn)
        .await
        .expect("query the grant matrix");
        assert_eq!(rows.len(), 20, "the contract schema changed shape");
        for row in &rows {
            let table: String = row.try_get("relname").expect("relname");
            let get = |col| -> bool { row.try_get(col).expect("bool") };
            assert!(get("sel"), "{table}: the kernel must be able to read it");
            assert!(!get("trunc"), "{table}: TRUNCATE is granted nowhere");
            match table.as_str() {
                // Append-only history: writable forward, never rewritable.
                // `ingested_record` and `cost_entry` sit here rather than with
                // the projections because a replay rebuilds them by INSERT
                // alone — no version to move, no state to advance.
                "event" | "receipt" | "ingested_record" | "cost_entry" => {
                    assert!(get("ins"), "{table}: history must be appendable");
                    assert!(!get("upd"), "{table}: history must not be rewritable");
                    assert!(!get("del"), "{table}: DELETE is granted nowhere else");
                }
                // The contract ships this seed; the kernel only reads it.
                "transition" => {
                    assert!(!get("ins"), "{table}: the FSM seed must not be writable");
                    assert!(!get("upd"), "{table}: the FSM seed must not be writable");
                    assert!(!get("del"), "{table}: DELETE is granted nowhere else");
                }
                // The one contract table where DELETE is real: a close removes
                // the row because the entity has no closed state — the log is
                // the history and a replay reproduces the same deletion.
                "workspace_node" => {
                    assert!(get("ins") && get("upd"), "{table}: the tree is rebuilt");
                    assert!(get("del"), "{table}: close is a real DELETE");
                }
                other => {
                    assert!(
                        write_tables.contains(&other),
                        "{other}: a new table appeared with no declared grant class"
                    );
                    assert!(get("ins") && get("upd"), "{other}: projections are rebuilt");
                    assert!(!get("del"), "{other}: DELETE is granted nowhere else");
                }
            }
        }
        conn.close().await.expect("close");
    }

    {
        let pool = PgPool::connect(&url_for(&occupied)).await.expect("connect");
        sqlx::raw_sql("CREATE TABLE legacy_users (id int primary key);")
            .execute(&pool)
            .await
            .expect("occupy the database");
        let err = admin::init(&pool, &admin_config(&occupied))
            .await
            .expect_err("a nonempty unrecognized database must be refused");
        let message = err.to_string();
        assert!(message.contains("legacy_users"), "{message}");
        assert!(message.contains("does not recognize"), "{message}");
        // The refusal is total: nothing was written on the way to it.
        let gwk_present: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = 'gwk')")
                .fetch_one(&pool)
                .await
                .expect("query pg_namespace");
        assert!(!gwk_present, "a refused init still created the gwk schema");
    }

    drop_database(&maintenance, &fresh).await;
    drop_database(&maintenance, &occupied).await;
}
