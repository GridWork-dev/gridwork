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

/// The runtime role for one test.
///
/// Per test, because a ROLE is cluster-scoped while a database is not. One
/// shared name is one shared object every test in this file races to create,
/// and on a cluster that does not already carry it the losers get 42710. This
/// file was safe only for as long as it held exactly one test; it no longer
/// does. An existence check would convert that loud failure into several tests
/// concurrently granting and revoking on one role — which is the privilege
/// matrix these tests exist to verify.
fn role_for(test: &str) -> String {
    format!("gwk_init_role_{}_{test}", std::process::id())
}

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

fn admin_config(database: &str, role: &str) -> AdminConfig {
    let url = url_for(database);
    let role = role.to_owned();
    AdminConfig::from_lookup(move |key| match key {
        ADMIN_DATABASE_URL_ENV => Some(url.clone()),
        RUNTIME_ROLE_ENV => Some(role.clone()),
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

/// Create this test's runtime role, replacing a leftover of the same name.
///
/// The role is still created by the harness and never by `init` — that part of
/// the contract is unchanged. What changed is that each test owns its own.
async fn create_role(maintenance: &PgPool, role: &str) {
    drop_role(maintenance, role).await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE ROLE {role} NOLOGIN;")))
        .execute(maintenance)
        .await
        .unwrap_or_else(|err| panic!("create the runtime role {role}: {err}"));
}

/// Drop a runtime role. AFTER its databases: PostgreSQL refuses to drop a role
/// that still holds privileges anywhere in the cluster, and a role outliving
/// its test is a cluster-level leak that accumulates on a long-lived
/// development container.
async fn drop_role(maintenance: &PgPool, role: &str) {
    let _ = sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP ROLE IF EXISTS {role};")))
        .execute(maintenance)
        .await;
}

async fn maintenance_pool() -> PgPool {
    PgPool::connect(&maintenance_url())
        .await
        .expect("connect to the maintenance database")
}

/// A freshly initialized database and the role granted on it.
async fn initialized_database(maintenance: &PgPool, test: &str) -> (String, String) {
    let role = role_for(test);
    create_role(maintenance, &role).await;
    let database = fresh_database(maintenance, test).await;
    let pool = PgPool::connect(&url_for(&database)).await.expect("connect");
    assert_eq!(
        admin::init(&pool, &admin_config(&database, &role))
            .await
            .expect("init"),
        InitOutcome::Initialized
    );
    (database, role)
}

/// A connection to `database` that has assumed `role`.
async fn as_runtime_role(database: &str, role: &str) -> PgConnection {
    let mut conn = PgConnection::connect(&url_for(database))
        .await
        .expect("dedicated connection");
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("SET ROLE {role};")))
        .execute(&mut conn)
        .await
        .expect("assume the runtime role");
    conn
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn init_applies_the_contract_is_idempotent_and_refuses_a_stranger() {
    let maintenance = maintenance_pool().await;

    // The runtime role is created by the operator, never by init.
    let role = role_for("applies");
    create_role(&maintenance, &role).await;

    let fresh = fresh_database(&maintenance, "fresh").await;
    let occupied = fresh_database(&maintenance, "occupied").await;

    {
        let pool = PgPool::connect(&url_for(&fresh)).await.expect("connect");
        assert_eq!(
            admin::inspect(&pool).await.expect("inspect"),
            TargetState::Empty
        );

        let config = admin_config(&fresh, &role);
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
        let mut conn = as_runtime_role(&fresh, &role).await;
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
            "pty_session",
            "pty_session_template",
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
        assert_eq!(rows.len(), 27, "the contract schema changed shape");
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
                //
                // The four Context truth records sit here because immutability
                // is what they ARE. A resolved manifest is the artifact an
                // independent verifier checks; one that can be edited afterward
                // proves nothing, since the row the verifier reads is no longer
                // the row the attempt ran against. They arrived UPDATE-able
                // from the blanket grant, which is how this assertion found
                // them. `context_blob` joins them on the same reasoning: a
                // rewritable classification would re-key or re-schedule a blob
                // out from under the sweep after the fact.
                "event"
                | "receipt"
                | "ingested_record"
                | "cost_entry"
                | "context_manifest"
                | "context_release"
                | "context_observation"
                | "context_finalization"
                | "context_blob" => {
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
        let delivery = sqlx::query(
            "SELECT has_table_privilege('gwk_internal.pty_delivery', 'SELECT') AS sel, \
                    has_table_privilege('gwk_internal.pty_delivery', 'INSERT') AS ins, \
                    has_table_privilege('gwk_internal.pty_delivery', 'UPDATE') AS upd, \
                    has_table_privilege('gwk_internal.pty_delivery', 'DELETE') AS del, \
                    has_table_privilege('gwk_internal.pty_delivery', 'TRUNCATE') AS trunc",
        )
        .fetch_one(&mut conn)
        .await
        .expect("query PTY delivery grants");
        assert!(delivery.try_get::<bool, _>("sel").expect("select"));
        assert!(delivery.try_get::<bool, _>("ins").expect("insert"));
        assert!(delivery.try_get::<bool, _>("upd").expect("update"));
        assert!(!delivery.try_get::<bool, _>("del").expect("delete"));
        assert!(!delivery.try_get::<bool, _>("trunc").expect("truncate"));
        conn.close().await.expect("close");
    }

    {
        let pool = PgPool::connect(&url_for(&occupied)).await.expect("connect");
        sqlx::raw_sql("CREATE TABLE legacy_users (id int primary key);")
            .execute(&pool)
            .await
            .expect("occupy the database");
        let err = admin::init(&pool, &admin_config(&occupied, &role))
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
    drop_role(&maintenance, &role).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn the_grant_matrix_over_both_schemas_is_declared_per_relation() {
    // The SAME assertion the migrate verb runs as R3, not a second copy of it.
    // Two folds is two places for a relation to be classified — or one place
    // for it to be classified and another where nobody noticed it was missing.
    // A fresh initialization and a migrated database now answer to one table.
    let maintenance = maintenance_pool().await;
    let (database, role) = initialized_database(&maintenance, "matrix2").await;
    let pool = PgPool::connect(&url_for(&database)).await.expect("connect");

    gwk_kernel::migrate::assert_grant_matrix(&pool, &role)
        .await
        .expect("the grant matrix holds over a freshly initialized database");

    drop(pool);
    drop_database(&maintenance, &database).await;
    drop_role(&maintenance, &role).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn the_internal_schema_grant_matrix_is_declared_per_relation() {
    // The sibling sweep over schema `gwk` has caught a wrongly-granted table
    // before — it is how #147's Context records were found UPDATE-able. It
    // reaches `gwk` only. `gwk_internal` got hand-written per-table blocks and
    // no count, so a new relation there could arrive with any grants at all
    // and no assertion in this file would look at it.
    //
    // The blanket `GRANT ... ON ALL TABLES IN SCHEMA gwk` does not reach
    // `gwk_internal`, so the default here is nothing rather than everything.
    // That is the safer default and it is not the point: the point is that
    // whatever a relation ends up with, some line in this file has to have
    // declared it.
    let maintenance = maintenance_pool().await;
    let (database, role) = initialized_database(&maintenance, "matrix").await;
    let mut conn = as_runtime_role(&database, &role).await;

    let rows = sqlx::query(
        "SELECT c.relname, \
           has_table_privilege(c.oid, 'SELECT')   AS sel, \
           has_table_privilege(c.oid, 'INSERT')   AS ins, \
           has_table_privilege(c.oid, 'UPDATE')   AS upd, \
           has_table_privilege(c.oid, 'DELETE')   AS del, \
           has_table_privilege(c.oid, 'TRUNCATE') AS trunc \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname = 'gwk_internal' AND c.relkind IN ('r', 'p', 'S', 'v', 'm') \
         ORDER BY 1",
    )
    .fetch_all(&mut conn)
    .await
    .expect("query the gwk_internal grant matrix");

    // Count first. A per-relation fold over a set that lost a relation agrees
    // with itself perfectly, and `all()` over an empty one is true — so the
    // number is asserted before anything iterates it, and a relation added
    // without a class declared below reds here rather than nowhere.
    assert_eq!(
        rows.len(),
        9,
        "the gwk_internal schema changed shape: {:?}",
        rows.iter()
            .map(|row| row.try_get::<String, _>("relname").expect("relname"))
            .collect::<Vec<_>>()
    );

    for row in &rows {
        let table: String = row.try_get("relname").expect("relname");
        let get = |column| -> bool { row.try_get(column).expect("bool") };
        assert!(
            !get("trunc"),
            "gwk_internal.{table}: TRUNCATE is granted nowhere"
        );
        match table.as_str() {
            // Read-only to the runtime role. Both of these are the kernel's
            // record of ITSELF — which contract this database carries, and how
            // it got here — and a process that could rewrite its own provenance
            // has none.
            "schema_fingerprint" | "schema_migration" => {
                assert!(get("sel"), "gwk_internal.{table}: the kernel reads it");
                assert!(
                    !get("ins"),
                    "gwk_internal.{table}: written by init and migration only"
                );
                assert!(
                    !get("upd"),
                    "gwk_internal.{table}: provenance must not be rewritable"
                );
                assert!(
                    !get("del"),
                    "gwk_internal.{table}: provenance must not be erasable"
                );
            }
            // The writer lease: one row, moved by CAS, never inserted or
            // removed by the runtime.
            "writer" => {
                assert!(
                    get("sel") && get("upd"),
                    "gwk_internal.writer: the lease moves"
                );
                assert!(
                    !get("ins"),
                    "gwk_internal.writer: the row is seeded, not created"
                );
                assert!(
                    !get("del"),
                    "gwk_internal.writer: a released lease is an update"
                );
            }
            // Durable delivery state: appended when a control is enqueued,
            // updated as it settles, never removed.
            "pty_delivery" => {
                assert!(
                    get("sel") && get("ins") && get("upd"),
                    "gwk_internal.pty_delivery"
                );
                assert!(
                    !get("del"),
                    "gwk_internal.pty_delivery: settling is an update"
                );
            }
            // The one place DELETE is real: sweep reclaims unreferenced blobs,
            // pins are released, uploads expire. None of that is history — the
            // events referencing a blob outlive its bytes.
            "blob" | "blob_pin" | "blob_upload" => {
                assert!(
                    get("sel") && get("ins") && get("upd"),
                    "gwk_internal.{table}"
                );
                assert!(get("del"), "gwk_internal.{table}: sweep is a real DELETE");
            }
            // Snapshots are appended and read; a stale one is superseded by a
            // newer row rather than edited.
            "checkpoint" => {
                assert!(get("sel") && get("ins"), "gwk_internal.checkpoint");
                assert!(
                    !get("upd"),
                    "gwk_internal.checkpoint: a snapshot is not edited"
                );
                assert!(
                    !get("del"),
                    "gwk_internal.checkpoint: DELETE is granted nowhere else"
                );
            }
            // The ledger's identity sequence. `GRANT ... ON ALL TABLES` never
            // covered sequences in any schema, and `gwk_internal` has no
            // blanket grant regardless, so it holds nothing — which is right,
            // because the role that cannot INSERT into the ledger has no use
            // for the counter behind it. Declared rather than filtered out of
            // the query: a sequence the runtime role could `setval` is a real
            // capability, and a guard that stops looking at sequences would
            // never see it.
            "schema_migration_seq_seq" => {
                for (privilege, held) in [
                    ("SELECT", get("sel")),
                    ("INSERT", get("ins")),
                    ("UPDATE", get("upd")),
                    ("DELETE", get("del")),
                ] {
                    assert!(
                        !held,
                        "gwk_internal.{table}: the runtime role holds {privilege} on a \
                         sequence it can never need"
                    );
                }
            }
            other => panic!(
                "gwk_internal.{other}: a new relation appeared with no declared grant class. \
                 Add its class here in the same commit that adds the relation — this arm is \
                 the only thing standing between a new internal table and whatever privileges \
                 it happens to inherit"
            ),
        }
    }

    conn.close().await.expect("close");
    drop_database(&maintenance, &database).await;
    drop_role(&maintenance, &role).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn the_runtime_role_may_read_the_ledger_and_nothing_more() {
    let maintenance = maintenance_pool().await;
    let (database, role) = initialized_database(&maintenance, "ledgergrant").await;
    let mut conn = as_runtime_role(&database, &role).await;

    let row = sqlx::query(
        "SELECT has_table_privilege('gwk_internal.schema_migration', 'SELECT')   AS sel, \
                has_table_privilege('gwk_internal.schema_migration', 'INSERT')   AS ins, \
                has_table_privilege('gwk_internal.schema_migration', 'UPDATE')   AS upd, \
                has_table_privilege('gwk_internal.schema_migration', 'DELETE')   AS del, \
                has_table_privilege('gwk_internal.schema_migration', 'TRUNCATE') AS trunc",
    )
    .fetch_one(&mut conn)
    .await
    .expect("query the ledger grants");
    let get = |column| -> bool { row.try_get(column).expect("bool") };

    // Readable, because the kernel reports what contract it carries and how it
    // got there. Nothing else, because a migration is not something the serving
    // process does.
    assert!(get("sel"), "the ledger must be readable");
    for (privilege, held) in [
        ("INSERT", get("ins")),
        ("UPDATE", get("upd")),
        ("DELETE", get("del")),
        ("TRUNCATE", get("trunc")),
    ] {
        assert!(!held, "the runtime role holds {privilege} on the ledger");
    }

    conn.close().await.expect("close");
    drop_database(&maintenance, &database).await;
    drop_role(&maintenance, &role).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn the_ledger_refuses_mutation_from_a_superuser() {
    // As SUPERUSER deliberately. Withholding a grant proves something about
    // the runtime role and nothing about anyone else, and the process that
    // applies a migration is not the runtime role. A superuser is bound by no
    // grant at all, so it is the only credential that can tell an append-only
    // TABLE apart from an append-only ROLE.
    let maintenance = maintenance_pool().await;
    let (database, role) = initialized_database(&maintenance, "ledgerguard").await;
    let pool = PgPool::connect(&url_for(&database)).await.expect("connect");

    let superuser: bool =
        sqlx::query_scalar("SELECT usesuper FROM pg_user WHERE usename = current_user")
            .fetch_one(&pool)
            .await
            .expect("query pg_user");
    assert!(
        superuser,
        "this test proves the trigger binds a credential no grant does; \
         running it as a non-superuser would prove the grants instead"
    );

    sqlx::query(
        "INSERT INTO gwk_internal.schema_migration \
           (base_sha256, result_sha256, step_id, backend_migrations) \
         VALUES ($1, $2, 'aaaaaaaa-bbbbbbbb.sql', $3)",
    )
    .bind("a".repeat(64))
    .bind("b".repeat(64))
    // A step that carried one. Empty would work and would exercise less: the
    // column is `text[] NOT NULL` with a validating CHECK, and a row that
    // carries nothing never reaches it.
    .bind(vec!["0005_pty_delivery".to_owned()])
    .execute(&pool)
    .await
    .expect("append a ledger row");

    let count = |pool: PgPool| async move {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM gwk_internal.schema_migration")
            .fetch_one(&pool)
            .await
            .expect("count ledger rows")
    };
    assert_eq!(count(pool.clone()).await, 1, "the row did not land");

    // Each refusal is asserted on its own, and the count is re-read after all
    // three. A battery that only checks the errors would pass against a table
    // that raised and rolled the row away anyway.
    for statement in [
        "UPDATE gwk_internal.schema_migration SET step_id = 'rewritten' WHERE seq = 1",
        "DELETE FROM gwk_internal.schema_migration WHERE seq = 1",
        "TRUNCATE gwk_internal.schema_migration",
    ] {
        let err = sqlx::raw_sql(sqlx::AssertSqlSafe(statement.to_owned()))
            .execute(&pool)
            .await
            .expect_err(&format!("{statement} must be refused"));
        let message = err.to_string();
        assert!(
            message.contains("append-only"),
            "{statement} failed for the wrong reason: {message}"
        );
    }

    assert_eq!(
        count(pool.clone()).await,
        1,
        "the row is gone after three refusals that all reported success"
    );

    drop(pool);
    drop_database(&maintenance, &database).await;
    drop_role(&maintenance, &role).await;
}
