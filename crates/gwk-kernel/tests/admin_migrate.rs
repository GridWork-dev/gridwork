//! Proves the retroactive contract step against a real PostgreSQL.
//!
//! The step in `schema/steps/` claims that a database initialized from the
//! contract at `4d54bba` — which is the one running in production — reaches the
//! contract this binary carries. That claim is only worth what it is tested
//! against, and it cannot be tested without a server: whether the DDL applies
//! at all, whether a decided gate row survives the CHECK that arrives with it,
//! and whether the result is the same schema a fresh initialization produces
//! are all questions only PostgreSQL can answer.
//!
//! ```text
//! docker run --rm -d -p 127.0.0.1:55432:5432 -e POSTGRES_HOST_AUTH_METHOD=trust \
//!   --name gwk-pg postgres:16
//! GWK_TEST_ADMIN_DATABASE_URL=postgres://postgres@localhost:55432/postgres \
//!   cargo test -p gwk-kernel --test admin_migrate -- --ignored
//! ```
//!
//! Trust auth, so the throwaway container needs no password and this file
//! carries no credential-shaped literal. That is only safe because the publish
//! is bound to loopback: `-p 55432:5432` would put a passwordless superuser on
//! every interface the host has.
//!
//! The base is rebuilt from git rather than from a restore of the live
//! database. `git show 4d54bba:schema/0001_contract.sql` digests to the exact
//! value the step's header names as its base, which is asserted below — so this
//! proof runs anywhere the repository does, and does not rest on an operator
//! sitting next to a production dump.

use gwk_kernel::admin::{self, InitOutcome};
use gwk_kernel::config::{ADMIN_DATABASE_URL_ENV, AdminConfig, RUNTIME_ROLE_ENV};
use gwk_kernel::contract_sql::CONTRACT_SQL_SHA256;
use gwk_kernel::contract_steps::{CONTRACT_STEPS, Step};
use gwk_kernel::migrate::Applied;
use sha2::{Digest, Sha256};
use sqlx::{Connection, PgConnection, PgPool, Row};

const ADMIN_URL_ENV: &str = "GWK_TEST_ADMIN_DATABASE_URL";

/// The runtime role for one test.
///
/// Per test, and that is not tidiness. Each test already builds its own
/// database, but a ROLE is CLUSTER-scoped — one shared name is one shared
/// object that all three tests race to create, and on a cluster that does not
/// already carry it two of them lose with 42710. The suite passed here for a
/// while because the container had been used before; a CI service container
/// never has.
///
/// Guarding the create with an existence check would fix that symptom and
/// leave three tests concurrently granting and revoking on one role — a loud
/// failure traded for a silent one, in the exact privilege matrix these tests
/// exist to verify. Separate roles make the isolation structural instead of a
/// question of how the runner happened to schedule.
///
/// Both databases in a test share the test's role deliberately: `pg_dump`
/// writes the grantee's name into every GRANT line, so two databases compared
/// line for line have to have been granted to the same one.
fn role_for(test: &str) -> String {
    format!("gwk_migrate_role_{}_{test}", std::process::id())
}

/// The revision the live database was initialized from.
const BASE_REVISION: &str = "4d54bba";

/// Its contract digest, and the step's declared base. Asserted against the
/// bytes git actually hands back, in [`base_contract_sql`].
const BASE_CONTRACT_SHA256: &str =
    "aba2f647bc7bb447e7b53307196f63df0bc718d479ec4693f6dd34ec9bf7b545";

/// Backend migrations as of [`BASE_REVISION`]. `0005_pty_delivery` is absent on
/// purpose — it arrives with #103, and bringing it is part of what the step is
/// being tested for.
const BASE_MIGRATIONS: [&str; 4] = [
    "0001_kernel_internal",
    "0002_writer",
    "0003_blob",
    "0004_checkpoint",
];

/// A fixed `\restrict` key for both dumps.
///
/// pg_dump stamps a random nonce into a `\restrict`/`\unrestrict` pair, which
/// would differ between any two dumps and force the comparison below to filter
/// them out. Fixing the key removes the difference at the source instead. That
/// matters more than it looks: a filter that can remove a line is a filter that
/// can remove a REAL line, and the comparison here is meant to have no such
/// escape hatch at all.
const RESTRICT_KEY: &str = "gwkmigratetestfixedkey";

fn maintenance_url() -> String {
    std::env::var(ADMIN_URL_ENV).unwrap_or_else(|_| {
        panic!("{ADMIN_URL_ENV} must point at a PostgreSQL superuser DSN for this test")
    })
}

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

fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("crates/gwk-kernel has two ancestors")
        .to_path_buf()
}

/// True when the checkout has no full history.
fn is_shallow_clone() -> bool {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_root())
        .arg("rev-parse")
        .arg("--is-shallow-repository")
        .output()
        .expect("run git rev-parse");
    output.status.success() && String::from_utf8_lossy(&output.stdout).trim() == "true"
}

/// One file as it stood at [`BASE_REVISION`].
fn git_show(path: &str) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_root())
        .arg("show")
        .arg(format!("{BASE_REVISION}:{path}"))
        .output()
        .expect("run git show");

    if !output.status.success() {
        // `fatal: invalid object name` is what a shallow clone says, and read
        // alone it points at the revision rather than at the checkout. Name the
        // real cause instead — every developer machine has full history, so the
        // one place this fires is CI, where nobody is watching it happen.
        //
        // A FAILURE, never a skip. This suite's whole claim is that the base is
        // reconstructible from the repository; a run that quietly stands down
        // when it cannot find the base reports success for the proof it did not
        // perform, which is worse than red.
        let cause = if is_shallow_clone() {
            "this checkout is SHALLOW and the suite reconstructs the base contract from \
             history — give the job `fetch-depth: 0`"
        } else {
            "the checkout has full history, so the revision itself is the problem"
        };
        panic!(
            "git show {BASE_REVISION}:{path} failed: {cause}. git said: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    String::from_utf8(output.stdout).expect("the tree is UTF-8")
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut acc, byte| {
            use std::fmt::Write as _;
            let _ = write!(acc, "{byte:02x}");
            acc
        })
}

/// The contract at [`BASE_REVISION`], proven to be the digest the step claims.
///
/// This assertion is the reason the whole proof is portable. If it holds, the
/// database this step reads from is reconstructible by anyone with the
/// repository, and the step is not resting on a claim about a machine nobody
/// else can see.
fn base_contract_sql() -> String {
    let sql = git_show("schema/0001_contract.sql");
    assert_eq!(
        sha256_hex(sql.as_bytes()),
        BASE_CONTRACT_SHA256,
        "the contract at {BASE_REVISION} is not the digest the step bases on"
    );
    sql
}

/// The grant matrix as it stood at [`BASE_REVISION`], reproduced from that
/// revision's `admin::backend_script`.
///
/// Only the grants a base database really carried matter here, and only in one
/// respect: the step reads the runtime role back off the database rather than
/// being told it, so a base with no granted role would make it refuse. The
/// step then replays the whole matrix, so an inexact reconstruction of the
/// historical privileges could not, by itself, make the comparison below pass
/// or fail — which is worth knowing when reading that comparison.
fn base_backend_script(role: &str) -> String {
    format!(
        "INSERT INTO gwk_internal.schema_fingerprint (id, contract_sha256) \
           VALUES (1, '{BASE_CONTRACT_SHA256}');\n\
         GRANT USAGE ON SCHEMA gwk TO {role};\n\
         GRANT USAGE ON SCHEMA gwk_internal TO {role};\n\
         GRANT SELECT, INSERT, UPDATE ON ALL TABLES IN SCHEMA gwk TO {role};\n\
         REVOKE UPDATE ON gwk.event, gwk.receipt, gwk.ingested_record, \
           gwk.cost_entry FROM {role};\n\
         REVOKE INSERT, UPDATE ON gwk.transition FROM {role};\n\
         GRANT DELETE ON gwk.workspace_node TO {role};\n\
         GRANT SELECT ON gwk_internal.schema_fingerprint TO {role};\n\
         GRANT SELECT, UPDATE ON gwk_internal.writer TO {role};\n\
         GRANT SELECT, INSERT, UPDATE, DELETE ON \
           gwk_internal.blob, gwk_internal.blob_pin, gwk_internal.blob_upload \
           TO {role};\n\
         GRANT SELECT, INSERT ON gwk_internal.checkpoint TO {role};\n"
    )
}

/// A database in the state the live one is in: initialized at the base
/// revision, and nothing since.
async fn build_base(pool: &PgPool, role: &str) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(base_contract_sql()))
        .execute(pool)
        .await
        .expect("apply the base contract");
    for migration in BASE_MIGRATIONS {
        let sql = git_show(&format!("crates/gwk-kernel/migrations/{migration}.sql"));
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
            .execute(pool)
            .await
            .unwrap_or_else(|err| panic!("apply backend migration {migration}: {err}"));
    }
    sqlx::raw_sql(sqlx::AssertSqlSafe(base_backend_script(role)))
        .execute(pool)
        .await
        .expect("apply the base grant matrix");
}

/// The step the registry says carries the live contract to this binary's.
///
/// Resolved rather than looked up by name, which makes this the one place the
/// whole mechanism is exercised end to end: the generator emitted the registry,
/// the resolver walked it, and what came back is what gets applied.
fn resolved_chain() -> Vec<&'static Step> {
    let chain =
        gwk_kernel::migrate::resolve(CONTRACT_STEPS, BASE_CONTRACT_SHA256, CONTRACT_SQL_SHA256)
            .unwrap_or_else(|refusal| {
                panic!("no chain from the live contract to this binary's: {refusal}")
            });
    assert_eq!(
        chain.len(),
        1,
        "expected exactly one step between {BASE_CONTRACT_SHA256} and {CONTRACT_SQL_SHA256}, \
         got {:?}",
        chain.iter().map(|step| step.id).collect::<Vec<_>>()
    );
    chain
}

/// Run the real `gw admin migrate` transaction against `pool`.
///
/// The applier, not a model of it. An earlier draft of this file applied the
/// step by hand and then replayed a filtered grant matrix of its own — which
/// meant the test could agree with itself while the function it was standing in
/// for was wrong, and it hid a whole half of the work (the backend migrations
/// no step's SQL contains) behind a constant the test maintained.
async fn migrate(pool: &PgPool, role: &str) -> Applied {
    let chain = resolved_chain();
    gwk_kernel::migrate::apply(
        pool,
        &chain,
        role,
        BASE_CONTRACT_SHA256,
        false,
        "0000000000000000000000000000000000000000",
        None,
    )
    .await
    .expect("apply the chain")
}

async fn recorded_contract(pool: &PgPool) -> String {
    sqlx::query_scalar("SELECT contract_sha256 FROM gwk_internal.schema_fingerprint WHERE id = 1")
        .fetch_one(pool)
        .await
        .expect("read the fingerprint")
}

fn pg_dump(database: &str) -> Vec<String> {
    let output = std::process::Command::new("pg_dump")
        .arg("--schema-only")
        .arg(format!("--restrict-key={RESTRICT_KEY}"))
        .arg("--dbname")
        .arg(url_for(database))
        .output()
        .expect("run pg_dump");
    assert!(
        output.status.success(),
        "pg_dump {database} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("pg_dump output is UTF-8")
        .lines()
        .map(str::to_owned)
        .collect()
}

/// The line range of `CREATE TABLE <qualified> (` through its closing `);`.
///
/// Asserts exactly one such block. The comparison below lifts a column line out
/// of a table body, and a column NAME is not unique across a schema — `gwk.gate`
/// and `gwk.cost_entry` both declare an `engine_session_id text,`. Lifting by
/// text alone would take both, and a difference in the one nobody was looking at
/// would vanish with it. Scoping to the block is what makes the lift mean the
/// column it names.
fn table_block(lines: &[String], qualified: &str, side: &str) -> std::ops::Range<usize> {
    let opener = format!("CREATE TABLE {qualified} (");
    let starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| **line == opener)
        .map(|(index, _)| index)
        .collect();
    assert_eq!(
        starts.len(),
        1,
        "{side}: expected exactly one {opener:?}, found {}",
        starts.len()
    );
    let start = starts[0];
    let body = lines[start + 1..]
        .iter()
        .position(|line| line == ");")
        .unwrap_or_else(|| panic!("{side}: the {qualified} block is unterminated"));
    start..start + body + 2
}

/// Remove each pattern from `lines`, asserting it appeared EXACTLY once.
///
/// Exactly, not at least: a pattern that matched twice would be a second
/// occurrence this comparison never looked at, and a pattern that matched none
/// would be a line the dump stopped emitting — both of which are the sort of
/// difference the comparison exists to find, not to absorb.
fn lift_once_each(lines: &[String], patterns: &[&str], side: &str) -> Vec<String> {
    let mut kept: Vec<String> = Vec::with_capacity(lines.len());
    let mut hits = vec![0usize; patterns.len()];
    for line in lines {
        match patterns.iter().position(|pattern| line == pattern) {
            Some(index) => hits[index] += 1,
            None => kept.push(line.clone()),
        }
    }
    for (pattern, count) in patterns.iter().zip(&hits) {
        assert_eq!(
            *count, 1,
            "{side}: expected {pattern:?} exactly once, found {count}"
        );
    }
    kept
}

/// Everything outside `blocks`, in order.
fn outside(lines: &[String], blocks: &[std::ops::Range<usize>]) -> Vec<String> {
    lines
        .iter()
        .enumerate()
        .filter(|(index, _)| !blocks.iter().any(|block| block.contains(index)))
        .map(|(_, line)| line.clone())
        .collect()
}

/// A uniquely named, freshly created, empty database. Dropped by the caller.
async fn fresh_database(maintenance: &PgPool, suffix: &str) -> String {
    let name = format!("gwk_migrate_{}_{suffix}", std::process::id());
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

async fn maintenance_pool() -> PgPool {
    PgPool::connect(&maintenance_url())
        .await
        .expect("connect to the maintenance database")
}

/// Create this test's runtime role, replacing a leftover of the same name.
///
/// Drop-then-create rather than create-if-absent, matching [`fresh_database`]:
/// the name is unique to this process and this test, so anything already
/// wearing it is residue from a run that panicked, and inheriting its grants
/// would be inheriting an unknown starting state.
async fn create_role(maintenance: &PgPool, role: &str) {
    drop_role(maintenance, role).await;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE ROLE {role} NOLOGIN;")))
        .execute(maintenance)
        .await
        .unwrap_or_else(|err| panic!("create the runtime role {role}: {err}"));
}

/// Drop a runtime role. Call AFTER the databases granted to it are gone —
/// PostgreSQL refuses to drop a role that still holds privileges anywhere in
/// the cluster, and a role that survives its test is a cluster-level leak that
/// accumulates on a long-lived development container.
async fn drop_role(maintenance: &PgPool, role: &str) {
    let _ = sqlx::raw_sql(sqlx::AssertSqlSafe(format!("DROP ROLE IF EXISTS {role};")))
        .execute(maintenance)
        .await;
}

/// Seed one decided gate. `verdict` is `'pass'` because the contract's own
/// `gate_verdict_check` admits only `pending`, `pass`, `fail`, and `partial` —
/// a decided gate in any other state is not a row this schema can hold.
async fn seed_decided_gate(pool: &PgPool, gate_id: &str) {
    sqlx::query("INSERT INTO gwk.gate (id, verdict) VALUES ($1, 'pass')")
        .bind(gate_id)
        .execute(pool)
        .await
        .expect("seed a decided gate");
}

/// Append one `gate_decided` event carrying `actor`.
async fn seed_gate_decided_event(pool: &PgPool, seq: i64, gate_id: &str, actor: &str) {
    sqlx::query(
        "INSERT INTO gwk.event (seq, event_id, project_id, aggregate_type, aggregate_id, \
           aggregate_version, event_type, schema_version, occurred_at, appended_at, actor, \
           origin, payload) \
         VALUES ($1::numeric, $2, 'proj-test', 'gate', $3, $1, 'gate_decided', 1, now(), now(), \
           $4::jsonb, '{}'::jsonb, '{}'::jsonb)",
    )
    .bind(seq)
    .bind(format!("evt-{seq}"))
    .bind(gate_id)
    .bind(actor)
    .execute(pool)
    .await
    .expect("seed a gate_decided event");
}

async fn gate_decided_by(pool: &PgPool, gate_id: &str) -> serde_json::Value {
    sqlx::query("SELECT decided_by FROM gwk.gate WHERE id = $1")
        .bind(gate_id)
        .fetch_one(pool)
        .await
        .expect("read the gate")
        .try_get::<serde_json::Value, _>("decided_by")
        .expect("decided_by is not null after the backfill")
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn the_retroactive_step_reaches_this_binarys_contract() {
    let maintenance = maintenance_pool().await;
    let role = role_for("reaches");
    create_role(&maintenance, &role).await;
    let migrated = fresh_database(&maintenance, "migrated").await;
    let initialized = fresh_database(&maintenance, "initialized").await;

    {
        let pool = PgPool::connect(&url_for(&migrated)).await.expect("connect");
        build_base(&pool, &role).await;
        assert_eq!(
            recorded_contract(&pool).await,
            BASE_CONTRACT_SHA256,
            "the reconstructed base does not record the contract it was built from"
        );
        let applied = migrate(&pool, &role).await;
        assert_eq!(
            recorded_contract(&pool).await,
            CONTRACT_SQL_SHA256,
            "the step applied but the database still reports the contract it left"
        );
        assert_eq!(applied.result, CONTRACT_SQL_SHA256);
        // The receipt distinguishes the two halves. Crediting the step with the
        // backend migrations would make the ledger claim the contract DDL
        // created relations it never mentions.
        assert_eq!(applied.steps.len(), 1, "{:?}", applied.steps);
        assert_eq!(
            applied.backend_migrations,
            ["0005_pty_delivery", "0006_schema_migration"],
            "the applier carried a different set than the step declares"
        );
        // The chain writes no events. A difference here is a daemon that was
        // not fenced, not a migration that logged something.
        assert_eq!(applied.events_before, applied.events_after);
    }
    {
        let pool = PgPool::connect(&url_for(&initialized))
            .await
            .expect("connect");
        assert_eq!(
            admin::init(&pool, &admin_config(&initialized, &role))
                .await
                .expect("init"),
            InitOutcome::Initialized
        );
    }

    let from_init = pg_dump(&initialized);
    let from_step = pg_dump(&migrated);

    // Nothing was added, dropped, or altered. Compared as multisets first,
    // because a line that MOVED is not a line that changed — and sorting is
    // what tells those two apart. A missing constraint, a widened type, an
    // ungranted table, an extra trigger: every one of them changes this.
    assert_eq!(
        from_init.len(),
        from_step.len(),
        "the two schemas do not even have the same number of lines"
    );
    let mut sorted_init = from_init.clone();
    let mut sorted_step = from_step.clone();
    sorted_init.sort();
    sorted_step.sort();
    assert_eq!(
        sorted_init, sorted_step,
        "the migrated schema and a fresh one do not carry the same set of lines"
    );

    // And nothing MOVED except the two columns that cannot be inserted in
    // place. PostgreSQL appends an added column and the fresh contract declares
    // both of these mid-table, so their ordinal position is the one difference
    // an ALTER cannot close — and it is a difference no code path can observe,
    // since every query in this crate names its columns.
    //
    // Closing it would mean rebuilding both tables: copy, drop, rename, and
    // recreate the CAS and append-only triggers on `pty_session`. That trade
    // buys nothing a caller can see and risks a guard silently not coming back,
    // so the step appends and this asserts the consequence exactly.
    //
    // Three assertions, because one would have to be vague. Inside each table:
    // lifting that table's appended column — once, asserted — leaves the body
    // identical in order. Outside both tables: identical in order, untouched.
    let gate_init = table_block(&from_init, "gwk.gate", "a fresh initialization");
    let gate_step = table_block(&from_step, "gwk.gate", "the migrated database");
    assert_eq!(
        lift_once_each(
            &from_init[gate_init.clone()],
            &["    decided_by jsonb,"],
            "a fresh gwk.gate"
        ),
        lift_once_each(
            &from_step[gate_step.clone()],
            &["    decided_by jsonb,"],
            "the migrated gwk.gate"
        ),
        "gwk.gate differs by more than where decided_by sits"
    );

    let pty_init = table_block(&from_init, "gwk.pty_session", "a fresh initialization");
    let pty_step = table_block(&from_step, "gwk.pty_session", "the migrated database");
    assert_eq!(
        lift_once_each(
            &from_init[pty_init.clone()],
            &["    engine_session_id text,"],
            "a fresh gwk.pty_session"
        ),
        lift_once_each(
            &from_step[pty_step.clone()],
            &["    engine_session_id text,"],
            "the migrated gwk.pty_session"
        ),
        "gwk.pty_session differs by more than where engine_session_id sits"
    );

    assert_eq!(
        outside(&from_init, &[gate_init, pty_init]),
        outside(&from_step, &[gate_step, pty_step]),
        "the schemas differ outside the two tables the step adds a column to"
    );

    drop_database(&maintenance, &migrated).await;
    drop_database(&maintenance, &initialized).await;
    drop_role(&maintenance, &role).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn a_decided_gate_row_survives_the_step() {
    let maintenance = maintenance_pool().await;
    let role = role_for("survives");
    create_role(&maintenance, &role).await;
    let database = fresh_database(&maintenance, "decided").await;
    let pool = PgPool::connect(&url_for(&database)).await.expect("connect");

    build_base(&pool, &role).await;
    seed_decided_gate(&pool, "gate-decided").await;
    seed_gate_decided_event(&pool, 1, "gate-decided", r#"{"kind": "operator"}"#).await;

    // Without the backfill this is where the step dies: #103's CHECK demands a
    // `decided_by` on every non-pending row, and PostgreSQL validates an added
    // CHECK against the rows already there.
    migrate(&pool, &role).await;

    assert_eq!(
        gate_decided_by(&pool, "gate-decided").await,
        serde_json::json!({"kind": "operator"}),
        "the decided gate lost the actor the log still records"
    );

    drop(pool);
    drop_database(&maintenance, &database).await;
    drop_role(&maintenance, &role).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn a_regraded_gate_takes_its_latest_decision() {
    let maintenance = maintenance_pool().await;
    let role = role_for("regraded");
    create_role(&maintenance, &role).await;
    let database = fresh_database(&maintenance, "regraded").await;
    let pool = PgPool::connect(&url_for(&database)).await.expect("connect");

    build_base(&pool, &role).await;
    seed_decided_gate(&pool, "gate-regraded").await;
    // Two decisions on one gate, by different actors. The projector treats a
    // re-decide as a whole new decision whose write wins, so the row describes
    // the LATER one — and a backfill that joins without qualifying which event
    // it means lets the server return either. That is non-determinism in the
    // one place a migration cannot have it, and the previous test cannot see
    // it: with a single event, both spellings agree.
    seed_gate_decided_event(&pool, 1, "gate-regraded", r#"{"kind": "operator"}"#).await;
    seed_gate_decided_event(&pool, 2, "gate-regraded", r#"{"kind": "engine"}"#).await;

    migrate(&pool, &role).await;

    assert_eq!(
        gate_decided_by(&pool, "gate-regraded").await,
        serde_json::json!({"kind": "engine"}),
        "the backfill took a superseded decision"
    );

    drop(pool);
    drop_database(&maintenance, &database).await;
    drop_role(&maintenance, &role).await;
}

/// The ledger row this migration wrote, as (base, result, step_id,
/// backend_migrations, asserted_base, backup_sha256).
async fn ledger_row(pool: &PgPool) -> (String, String, String, Vec<String>, bool, Option<String>) {
    let rows: Vec<_> = sqlx::query(
        "SELECT base_sha256, result_sha256, step_id, backend_migrations, asserted_base, \
                backup_sha256 \
           FROM gwk_internal.schema_migration ORDER BY seq",
    )
    .fetch_all(pool)
    .await
    .expect("read the ledger");
    // Counted before it is read out of: `fetch_one` on an empty ledger is an
    // error with a worse message, and on a ledger with two rows it would
    // silently describe one of them.
    assert_eq!(rows.len(), 1, "expected exactly one ledger row");
    let row = &rows[0];
    (
        row.try_get("base_sha256").expect("base"),
        row.try_get("result_sha256").expect("result"),
        row.try_get("step_id").expect("step_id"),
        row.try_get("backend_migrations")
            .expect("backend_migrations"),
        row.try_get("asserted_base").expect("asserted_base"),
        row.try_get("backup_sha256").expect("backup_sha256"),
    )
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn a_carried_backend_migration_lands_is_recorded_and_is_reachable() {
    // The half no step's SQL contains. `gwk_internal` has no digest and no
    // chain, so a migration added after a database was initialized arrives only
    // because the applier carried it — and if it arrives without its grants it
    // is a relation the runtime role cannot see, which nothing about the
    // database looks wrong about.
    let maintenance = maintenance_pool().await;
    let role = role_for("carried");
    create_role(&maintenance, &role).await;
    let database = fresh_database(&maintenance, "carried").await;
    let pool = PgPool::connect(&url_for(&database)).await.expect("connect");

    build_base(&pool, &role).await;
    // The base predates both carried migrations, which is the state this is
    // about: asserted rather than assumed, since a base that already had them
    // would make everything below vacuous.
    for relation in ["pty_delivery", "schema_migration"] {
        let present: bool =
            sqlx::query_scalar("SELECT to_regclass('gwk_internal.' || $1) IS NOT NULL")
                .bind(relation)
                .fetch_one(&pool)
                .await
                .expect("probe the base");
        assert!(!present, "the base already has gwk_internal.{relation}");
    }

    let applied = migrate(&pool, &role).await;

    assert_eq!(
        applied.backend_migrations,
        ["0005_pty_delivery", "0006_schema_migration"]
    );

    let (base, result, step_id, carried, asserted, backup) = ledger_row(&pool).await;
    assert_eq!(base, BASE_CONTRACT_SHA256);
    assert_eq!(result, CONTRACT_SQL_SHA256);
    assert_eq!(step_id, "aba2f647-7ebb2ada.sql");
    assert_eq!(
        carried,
        ["0005_pty_delivery", "0006_schema_migration"],
        "the row does not name what the run carried"
    );
    assert!(!asserted, "a plain run did not assert its base");
    assert_eq!(backup, None);

    // Reachable, and counted. "One query succeeded" is what a grant on one
    // table proves; the question is whether every relation the run created got
    // one, and only a count answers that.
    let mut conn = PgConnection::connect(&url_for(&database))
        .await
        .expect("dedicated connection");
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("SET ROLE {role};")))
        .execute(&mut conn)
        .await
        .expect("assume the runtime role");
    let readable: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
          WHERE n.nspname = 'gwk_internal' AND c.relkind = 'r' \
            AND has_table_privilege(c.oid, 'SELECT')",
    )
    .fetch_one(&mut conn)
    .await
    .expect("count readable gwk_internal tables");
    assert_eq!(
        readable, 8,
        "every gwk_internal table the run left behind must be readable by the runtime role"
    );
    // And the ledger specifically, since it is the one this commit adds.
    let ledger_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM gwk_internal.schema_migration")
        .fetch_one(&mut conn)
        .await
        .expect("the runtime role reads the ledger");
    assert_eq!(ledger_rows, 1);
    conn.close().await.expect("close");

    drop(pool);
    drop_database(&maintenance, &database).await;
    drop_role(&maintenance, &role).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn an_asserted_base_is_recorded_as_asserted() {
    // `--from` is the one path where the applier does not check the
    // precondition it acts on. The row is the only thing that will ever say so,
    // and it carries the digest the database ACTUALLY recorded beside the flag
    // — otherwise a later reader cannot tell which claim was overridden.
    let maintenance = maintenance_pool().await;
    let role = role_for("asserted");
    create_role(&maintenance, &role).await;
    let database = fresh_database(&maintenance, "asserted").await;
    let pool = PgPool::connect(&url_for(&database)).await.expect("connect");

    build_base(&pool, &role).await;
    let chain = resolved_chain();
    let applied = gwk_kernel::migrate::apply(
        &pool,
        &chain,
        &role,
        BASE_CONTRACT_SHA256,
        true,
        "0000000000000000000000000000000000000000",
        Some(&"c".repeat(64)),
    )
    .await
    .expect("apply with an asserted base");
    assert_eq!(applied.result, CONTRACT_SQL_SHA256);

    let (_, _, _, _, asserted, backup) = ledger_row(&pool).await;
    assert!(asserted, "the override left no trace in the ledger");
    assert_eq!(
        backup,
        Some("c".repeat(64)),
        "the backup digest the verb computed is not what the row records"
    );

    drop(pool);
    drop_database(&maintenance, &database).await;
    drop_role(&maintenance, &role).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn r1_refuses_a_database_already_at_this_binarys_contract() {
    // Criterion 3's mutation, shipped as the test. A fresh `admin init` is
    // already at the target; the resolver answers an equal pair with an empty
    // chain, so without R1 the refusal is "refusing to apply an empty chain" —
    // a true sentence about the wrong subject, arriving after the writer lock.
    let maintenance = maintenance_pool().await;
    let role = role_for("r1already");
    create_role(&maintenance, &role).await;
    let database = fresh_database(&maintenance, "r1already").await;
    let pool = PgPool::connect(&url_for(&database)).await.expect("connect");
    build_base(&pool, &role).await;
    migrate(&pool, &role).await;

    let refusal = gwk_kernel::migrate::assert_base(&pool, BASE_CONTRACT_SHA256)
        .await
        .expect_err("already at the target");
    let message = refusal.to_string();
    assert!(message.contains("nothing to migrate"), "{message}");
    assert!(message.contains(CONTRACT_SQL_SHA256), "{message}");

    // And it applied nothing: still exactly the one ledger row the migration
    // above wrote.
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM gwk_internal.schema_migration")
        .fetch_one(&pool)
        .await
        .expect("count ledger rows");
    assert_eq!(rows, 1, "the refused rung left a second row behind");

    drop(pool);
    drop_database(&maintenance, &database).await;
    drop_role(&maintenance, &role).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn r1_refuses_a_base_the_database_is_not_at_and_an_uninitialized_one() {
    let maintenance = maintenance_pool().await;
    let role = role_for("r1wrong");
    create_role(&maintenance, &role).await;
    let database = fresh_database(&maintenance, "r1wrong").await;
    let pool = PgPool::connect(&url_for(&database)).await.expect("connect");

    build_base(&pool, &role).await;
    let refusal = gwk_kernel::migrate::assert_base(&pool, &"d".repeat(64))
        .await
        .expect_err("a base this database is not at");
    let message = refusal.to_string();
    assert!(message.contains(BASE_CONTRACT_SHA256), "{message}");
    assert!(message.contains(&"d".repeat(64)), "{message}");

    // No fingerprint row at all: a database the kernel never initialized.
    sqlx::raw_sql("DELETE FROM gwk_internal.schema_fingerprint;")
        .execute(&pool)
        .await
        .expect("clear the fingerprint");
    let refusal = gwk_kernel::migrate::assert_base(&pool, BASE_CONTRACT_SHA256)
        .await
        .expect_err("no fingerprint row");
    assert!(
        refusal.to_string().contains("never been initialized"),
        "{refusal}"
    );

    drop(pool);
    drop_database(&maintenance, &database).await;
    drop_role(&maintenance, &role).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn r4_goes_red_when_a_protection_is_disabled() {
    // RED case 3, and the arm that proves the battery has a subject. A
    // protections battery that stays green after the trigger is disabled is
    // testing the grants — which bind the runtime role, not the superuser that
    // just applied a migration.
    let maintenance = maintenance_pool().await;
    let role = role_for("r4");
    create_role(&maintenance, &role).await;
    let database = fresh_database(&maintenance, "r4").await;
    let pool = PgPool::connect(&url_for(&database)).await.expect("connect");

    build_base(&pool, &role).await;
    migrate(&pool, &role).await;

    // Green on the migrated database.
    gwk_kernel::migrate::assert_protections(&pool)
        .await
        .expect("the protections hold after a migration");

    sqlx::raw_sql("ALTER TABLE gwk.event DISABLE TRIGGER event_no_truncate;")
        .execute(&pool)
        .await
        .expect("disable the truncate guard");
    let refusal = gwk_kernel::migrate::assert_protections(&pool)
        .await
        .expect_err("a disabled guard must red the battery");
    let message = refusal.to_string();
    assert!(
        message.contains("TRUNCATE gwk.event succeeded"),
        "{message}"
    );

    sqlx::raw_sql("ALTER TABLE gwk.event ENABLE ALWAYS TRIGGER event_no_truncate;")
        .execute(&pool)
        .await
        .expect("restore the truncate guard");

    // The row-level arm, separately: the two guards are different objects and a
    // battery that only proved one would report the other.
    sqlx::raw_sql("ALTER TABLE gwk.transition DISABLE TRIGGER transition_immutable;")
        .execute(&pool)
        .await
        .expect("disable the delete guard");
    let refusal = gwk_kernel::migrate::assert_protections(&pool)
        .await
        .expect_err("a disabled row guard must red the battery");
    assert!(
        refusal.to_string().contains("DELETE FROM gwk.transition"),
        "{refusal}"
    );

    drop(pool);
    drop_database(&maintenance, &database).await;
    drop_role(&maintenance, &role).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn dispatch_node_is_truncate_protected_only_by_a_neighbour() {
    // Found by R4 while it was being written, and pinned here because it is
    // true and surprising rather than because it is wrong.
    //
    // `gwk.dispatch_node` carries a row-level DELETE guard and NO
    // statement-level TRUNCATE cover — the exact pairing this repository's
    // schema comments warn about. It is protected anyway, but by accident: a
    // bare TRUNCATE is refused by the foreign key `gwk.cost_entry` holds on it,
    // and TRUNCATE ... CASCADE is refused by `cost_entry`'s OWN guard. Neither
    // refusal comes from dispatch_node.
    //
    // So the protection is real and it is one edit away from gone. This test is
    // what makes that edit loud.
    let maintenance = maintenance_pool().await;
    let role = role_for("dispatch");
    create_role(&maintenance, &role).await;
    let database = fresh_database(&maintenance, "dispatch").await;
    let pool = PgPool::connect(&url_for(&database)).await.expect("connect");
    build_base(&pool, &role).await;
    migrate(&pool, &role).await;

    let guards: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
          WHERE NOT t.tgisinternal AND t.tgtype & 32 > 0 \
            AND c.oid = 'gwk.dispatch_node'::regclass",
    )
    .fetch_one(&pool)
    .await
    .expect("count dispatch_node truncate guards");
    assert_eq!(
        guards, 0,
        "dispatch_node grew its own TRUNCATE guard — good; \
                            delete this test and add it to the battery"
    );

    let bare = sqlx::raw_sql("TRUNCATE gwk.dispatch_node;")
        .execute(&pool)
        .await
        .expect_err("a bare truncate");
    assert!(
        bare.to_string().contains("foreign key"),
        "refused, but not by the foreign key this rests on: {bare}"
    );

    let cascade = sqlx::raw_sql("TRUNCATE gwk.dispatch_node CASCADE;")
        .execute(&pool)
        .await
        .expect_err("a cascading truncate");
    assert!(
        cascade
            .to_string()
            .contains("gwk.cost_entry is append-only"),
        "refused, but not by cost_entry's guard: {cascade}"
    );

    drop(pool);
    drop_database(&maintenance, &database).await;
    drop_role(&maintenance, &role).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn a_failing_step_moves_neither_the_fingerprint_nor_the_ledger() {
    // Criterion 4. Both, because a fingerprint that held while a ledger row
    // leaked would be a receipt claiming a migration that did not happen — and
    // the ledger is append-only, so the claim would be permanent.
    let maintenance = maintenance_pool().await;
    let role = role_for("failing");
    create_role(&maintenance, &role).await;
    let database = fresh_database(&maintenance, "failing").await;
    let pool = PgPool::connect(&url_for(&database)).await.expect("connect");
    build_base(&pool, &role).await;

    let chain = resolved_chain();
    let poisoned = Step {
        sql: Box::leak(
            format!("{}\nSELECT this_function_does_not_exist();\n", chain[0].sql).into_boxed_str(),
        ),
        ..*chain[0]
    };
    let refusal = gwk_kernel::migrate::apply(
        &pool,
        &[&poisoned],
        &role,
        BASE_CONTRACT_SHA256,
        false,
        "0000000000000000000000000000000000000000",
        None,
    )
    .await
    .expect_err("a step that cannot run");
    assert!(
        refusal.to_string().contains("this_function_does_not_exist"),
        "{refusal}"
    );

    assert_eq!(
        recorded_contract(&pool).await,
        BASE_CONTRACT_SHA256,
        "the fingerprint moved under a step that failed"
    );
    // The ledger table does not exist yet on a base-shaped database, and that
    // is the correct answer to "did a row land": it could not have.
    let ledger_exists: bool =
        sqlx::query_scalar("SELECT to_regclass('gwk_internal.schema_migration') IS NOT NULL")
            .fetch_one(&pool)
            .await
            .expect("probe the ledger");
    assert!(
        !ledger_exists,
        "the failed transaction left the ledger table behind"
    );

    drop(pool);
    drop_database(&maintenance, &database).await;
    drop_role(&maintenance, &role).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn r3_and_r5_hold_over_a_migrated_database() {
    let maintenance = maintenance_pool().await;
    let role = role_for("r3r5");
    create_role(&maintenance, &role).await;
    let database = fresh_database(&maintenance, "r3r5").await;
    let pool = PgPool::connect(&url_for(&database)).await.expect("connect");
    build_base(&pool, &role).await;
    let applied = migrate(&pool, &role).await;

    gwk_kernel::migrate::assert_grant_matrix(&pool, &role)
        .await
        .expect("the grant matrix holds over both schemas after a migration");
    gwk_kernel::migrate::assert_result(&pool, &applied)
        .await
        .expect("the database ends where the chain said");

    drop(pool);
    drop_database(&maintenance, &database).await;
    drop_role(&maintenance, &role).await;
}
