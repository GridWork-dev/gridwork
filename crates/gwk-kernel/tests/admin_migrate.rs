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

async fn checkpoint_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM gwk_internal.checkpoint")
        .fetch_one(pool)
        .await
        .expect("count the checkpoints")
}

/// Seed one checkpoint at `through_seq`, in the shape the kernel writes.
///
/// Valid rather than merely insertable: the table's four CHECKs all hold, and
/// `records_ref` carries a real `PayloadRef` with `byte_size` as the decimal
/// string that type serializes to. The applier deletes by table and would clear
/// a malformed row just as happily — but a fixture the production writer could
/// not have produced would make the test's subject a row that never exists.
async fn seed_checkpoint(pool: &PgPool, through_seq: i64) {
    sqlx::query(
        "INSERT INTO gwk_internal.checkpoint \
           (through_seq, schema_version, projection_hash, records_ref, created_at) \
         VALUES ($1::numeric, 1, $2, $3::jsonb, now())",
    )
    .bind(through_seq)
    .bind("a".repeat(64))
    .bind(format!(
        r#"{{"digest": "sha256:{}", "media_type": "application/x-ndjson", "byte_size": "4096"}}"#,
        "a".repeat(64)
    ))
    .execute(pool)
    .await
    .expect("seed a checkpoint");
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
/// backend_migrations, backup_sha256).
async fn ledger_row(pool: &PgPool) -> (String, String, String, Vec<String>, Option<String>) {
    let rows: Vec<_> = sqlx::query(
        "SELECT base_sha256, result_sha256, step_id, backend_migrations, \
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

    let (base, result, step_id, carried, backup) = ledger_row(&pool).await;
    assert_eq!(base, BASE_CONTRACT_SHA256);
    assert_eq!(result, CONTRACT_SQL_SHA256);
    assert_eq!(step_id, "aba2f647-7ebb2ada.sql");
    assert_eq!(
        carried,
        ["0005_pty_delivery", "0006_schema_migration"],
        "the row does not name what the run carried"
    );
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
async fn a_stated_base_is_checked_like_any_other_and_never_overrides() {
    // What `--from` really does, pinned, because three documents said it did
    // something else. It states the base the operator BELIEVES the database is
    // at; `assert_base` then compares that against the recorded fingerprint and
    // refuses on a mismatch. There is no path on which a stated base is applied
    // unchecked, so the ledger no longer carries a column claiming there is.
    //
    // The two arms below are the whole claim: a stated base that is wrong is
    // refused, and a stated base that is right is indistinguishable from having
    // stated nothing. A test asserting only the second would pass against a
    // build where `--from` bypassed the check entirely.
    let maintenance = maintenance_pool().await;
    let role = role_for("stated");
    create_role(&maintenance, &role).await;
    let database = fresh_database(&maintenance, "stated").await;
    let pool = PgPool::connect(&url_for(&database)).await.expect("connect");

    build_base(&pool, &role).await;

    // A base the database is not at. Well formed, and a digest no step bases
    // on would be refused by the resolver instead — this one has to reach
    // `assert_base` to prove `assert_base` is what refuses it.
    let refusal = gwk_kernel::migrate::assert_base(&pool, &"f".repeat(64))
        .await
        .expect_err("a stated base that is not the recorded one must be refused");
    let message = refusal.to_string();
    assert!(
        message.contains(BASE_CONTRACT_SHA256) && message.contains(&"f".repeat(64)),
        "the refusal names neither what the database records nor what was stated: {message}"
    );

    // And the true one passes, so the arm above is refusing the mismatch rather
    // than refusing everything.
    gwk_kernel::migrate::assert_base(&pool, BASE_CONTRACT_SHA256)
        .await
        .expect("the recorded base is the one the database is at");

    let chain = resolved_chain();
    let applied = gwk_kernel::migrate::apply(
        &pool,
        &chain,
        &role,
        BASE_CONTRACT_SHA256,
        "0000000000000000000000000000000000000000",
        Some(&"c".repeat(64)),
    )
    .await
    .expect("apply");
    assert_eq!(applied.result, CONTRACT_SQL_SHA256);

    let (_, _, _, _, backup) = ledger_row(&pool).await;
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
    // The COUNT arm, and naming which arm is the point of the assertion.
    // `DISABLE TRIGGER` leaves the `pg_trigger` row in place with `tgenabled`
    // set to 'D', so a sweep that filtered on nothing but `tgisinternal` and
    // `tgtype` counted it and moved on — the probe was the only thing that
    // could see a disabled guard, and for the three relations another table's
    // foreign key answers for, not even the probe could. Filtering the sweep to
    // `tgenabled = 'A'` drops a disabled guard out of the set, so the count
    // falls to 17 and refuses before any relation is probed.
    assert!(
        message.contains("expected exactly 18 relations with a TRUNCATE guard and found 17"),
        "a disabled guard must be caught by the count arm, not left to the probe: {message}"
    );
    assert!(
        !message.contains("gwk\", \"event"),
        "the relation whose guard was disabled is still in the counted set: {message}"
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
    let message = refusal.to_string();
    // The count arm again, and which arm answers is the assertion. The probe
    // used to be the only thing that could see this, and it saw it here only
    // because the contract seeds `gwk.transition`: `DELETE FROM t` over an empty
    // table fires no row-level trigger and succeeds, so on any guarded relation
    // that is empty when a migration runs — which is most of them — a disabled
    // guard was invisible. Counting the row-level guards as their own set
    // refuses one whether or not its table held a row to prove it with.
    assert!(
        message
            .contains("expected exactly 19 relations with a row-level DELETE guard and found 18"),
        "a disabled row guard must be caught by the count arm, not left to the probe: {message}"
    );
    assert!(
        !message.contains("gwk\", \"transition"),
        "the relation whose guard was disabled is still in the counted set: {message}"
    );

    drop(pool);
    drop_database(&maintenance, &database).await;
    drop_role(&maintenance, &role).await;
}

/// How many relations carry a statement-level TRUNCATE guard, counted with the
/// battery's own query.
///
/// The battery's query verbatim, not a re-derivation of it. A count assembled
/// some other way could agree with the constant while the set the battery
/// actually walks had changed underneath it, which is the failure this whole
/// test exists to make visible rather than a second copy of it.
async fn truncate_guarded_relations(pool: &PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM (\
           SELECT 1 FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
            WHERE NOT t.tgisinternal AND t.tgtype & 32 > 0 \
              AND t.tgenabled = 'A' \
              AND n.nspname IN ('gwk', 'gwk_internal') \
            GROUP BY n.nspname, c.relname) s",
    )
    .fetch_one(pool)
    .await
    .expect("count truncate-guarded relations")
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn r4_goes_red_when_a_guard_is_present_enabled_and_no_longer_refuses() {
    // The hole the count arm cannot reach, on the relation that hid it.
    //
    // PostgreSQL checks a table's inbound foreign keys BEFORE it fires any
    // BEFORE TRUNCATE trigger. `gwk.attempt` is referenced by five other tables,
    // so a bare `TRUNCATE gwk.attempt` is refused with 0A000 whether its guard
    // is there or not — measured, not reasoned: with the guard present the error
    // is the foreign key's, and the guard never runs. The probe arm therefore
    // never exercised this relation's guard in any run, and a battery reading
    // `is_err()` as proof reported it protected on the strength of somebody
    // else's constraint.
    //
    // The count arm does not cover this case. A trigger that is present,
    // enabled, and ALWAYS is counted — the mutation here leaves the catalogue
    // untouched and guts the FUNCTION, which is the one way a guard stops
    // guarding without the count moving.
    let maintenance = maintenance_pool().await;
    let role = role_for("gutted");
    create_role(&maintenance, &role).await;
    let database = fresh_database(&maintenance, "gutted").await;
    let pool = PgPool::connect(&url_for(&database)).await.expect("connect");

    build_base(&pool, &role).await;
    migrate(&pool, &role).await;

    gwk_kernel::migrate::assert_protections(&pool)
        .await
        .expect("the protections hold after a migration");
    let before = truncate_guarded_relations(&pool).await;
    assert_eq!(before, 18, "the mutation below must not move this count");

    // Present, enabled, ALWAYS — and it no longer refuses anything. A BEFORE
    // STATEMENT trigger cannot cancel by returning NULL, so the TRUNCATE
    // proceeds.
    sqlx::raw_sql(
        "CREATE OR REPLACE FUNCTION gwk.forbid_state_row_delete() RETURNS trigger \
           LANGUAGE plpgsql AS $gutted$ BEGIN RETURN NULL; END $gutted$;",
    )
    .execute(&pool)
    .await
    .expect("gut the guard function");

    assert_eq!(
        truncate_guarded_relations(&pool).await,
        before,
        "the mutation moved the count, so this test would pass for the wrong reason"
    );

    let refusal = gwk_kernel::migrate::assert_protections(&pool)
        .await
        .expect_err("a guard that no longer refuses must red the battery");
    let message = refusal.to_string();
    // What discrimination looks like here, and it is worth being precise about
    // because the mechanism is not the obvious one. The probe does not come back
    // "succeeded": CASCADE pulls `gwk.cost_entry` into the same statement and
    // ITS guard — a different function, untouched by this mutation — refuses.
    // The battery still reds, because a refusal naming another relation is not
    // evidence about this one. Present guard: `gwk.attempt`'s own trigger fires
    // first, because the target leads the truncate list, and the battery passes.
    // Gutted guard: the neighbour answers instead and the battery says so. Those
    // two outcomes differ, which is the whole requirement; a bare `is_err()`
    // collapses them into one and reports protection either way.
    assert!(
        message.contains("TRUNCATE gwk.attempt was refused, but not by its own guard"),
        "the battery must name gwk.attempt as the subject and report that something other than \
         its guard answered: {message}"
    );
    assert!(
        message.contains("gwk.cost_entry is append-only"),
        "the refusal must carry what actually answered, or the operator cannot tell a confounded \
         probe from a broken one: {message}"
    );

    drop(pool);
    drop_database(&maintenance, &database).await;
    drop_role(&maintenance, &role).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn r4_goes_red_when_a_truncate_guard_is_dropped() {
    // The direction the DISABLE test above cannot reach, and the reason it
    // cannot: `ALTER TABLE ... DISABLE TRIGGER` leaves the `pg_trigger` row in
    // place with `tgenabled = 'D'`, and the battery's query filters on neither
    // column — so the relation stays in the set, the per-relation probe still
    // asks about it, and the refusal comes from the probe. A DROP removes the
    // row, which removes the relation from the set, which means the probe
    // never asks. The COUNT is the only arm that can see it.
    let maintenance = maintenance_pool().await;
    let role = role_for("r4dropped");
    create_role(&maintenance, &role).await;
    let database = fresh_database(&maintenance, "r4dropped").await;
    let pool = PgPool::connect(&url_for(&database)).await.expect("connect");

    build_base(&pool, &role).await;
    migrate(&pool, &role).await;

    // The subject, established before it is mutated: green, over a set with a
    // known size. A test that only asserted the red would pass just as well
    // against a battery that was red for some other reason all along.
    let before = truncate_guarded_relations(&pool).await;
    gwk_kernel::migrate::assert_protections(&pool)
        .await
        .expect("the protections hold after a migration");

    sqlx::raw_sql("DROP TRIGGER event_no_truncate ON gwk.event;")
        .execute(&pool)
        .await
        .expect("drop the truncate guard");

    // The mutation landed, measured rather than assumed. A DROP that silently
    // did nothing would leave the battery green for the right reason, and this
    // test would then be asserting nothing at all.
    let after = truncate_guarded_relations(&pool).await;
    assert_eq!(
        after,
        before - 1,
        "the DROP removed {} guards, not exactly one",
        before - after
    );

    let refusal = gwk_kernel::migrate::assert_protections(&pool)
        .await
        .expect_err("a dropped guard must red the battery");
    let message = refusal.to_string();
    // The COUNT arm specifically. `TRUNCATE gwk.event succeeded` would mean the
    // per-relation probe caught it, which it cannot: gwk.event is no longer in
    // the set being walked. Naming the arm is what keeps this test honest if
    // the battery is ever restructured.
    assert!(
        message.contains(&format!("found {after}")),
        "the count arm is the refusal, and it reports what it found: {message}"
    );
    assert!(
        !message.contains("TRUNCATE gwk.event succeeded"),
        "a dropped guard is invisible to the per-relation probe, so it cannot be the refusal: \
         {message}"
    );

    drop(pool);
    drop_database(&maintenance, &database).await;
    drop_role(&maintenance, &role).await;
}

/// Re-add `pty_session_closed_iff_terminal` with `body`, or drop it entirely
/// when `body` is `None`.
///
/// One helper for both mutations, because they are the same edit at two depths
/// and the arm has to red on each: a constraint that came back WRONG and a
/// constraint that did not come back at all.
///
/// `IF EXISTS` because the restore below runs after the drop-outright mutation,
/// and it is the reason every call site asserts the resulting state with
/// [`closed_iff_terminal_present`] rather than trusting that the statement did
/// something — a DROP that silently matched nothing is exactly the mutation
/// that never happened.
async fn rewrite_closed_iff_terminal(pool: &PgPool, body: Option<&str>) {
    sqlx::raw_sql(
        "ALTER TABLE gwk.pty_session DROP CONSTRAINT IF EXISTS pty_session_closed_iff_terminal;",
    )
    .execute(pool)
    .await
    .expect("drop the closed-iff-terminal constraint");
    if let Some(body) = body {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "ALTER TABLE gwk.pty_session ADD CONSTRAINT pty_session_closed_iff_terminal \
             CHECK ({body});"
        )))
        .execute(pool)
        .await
        .expect("re-add the closed-iff-terminal constraint");
    }
}

/// Whether the constraint the arm probes is on the table at all, read out of
/// `pg_constraint` rather than inferred from the mutation having been issued.
async fn closed_iff_terminal_present(pool: &PgPool) -> bool {
    sqlx::query_scalar::<_, bool>(
        "SELECT count(*) = 1 FROM pg_constraint \
          WHERE conname = 'pty_session_closed_iff_terminal' \
            AND conrelid = 'gwk.pty_session'::regclass",
    )
    .fetch_one(pool)
    .await
    .expect("probe the constraint")
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn r4_goes_red_when_the_closed_iff_constraint_comes_back_one_directional() {
    // Criterion 6's half-closed UPDATE arm, and the reason it is worth having
    // separately from the TRUNCATE and DELETE arms: the migration step DROPS
    // `pty_session_closed_iff_terminal` and re-ADDs it, and a re-add that turns
    // the iff into a one-way implication is accepted by PostgreSQL, validates
    // against every existing row, and refuses exactly half of what its name
    // says. Nothing above this can see that — TRUNCATE and DELETE do not
    // evaluate a CHECK.
    let maintenance = maintenance_pool().await;
    let role = role_for("r4iff");
    create_role(&maintenance, &role).await;
    let database = fresh_database(&maintenance, "r4iff").await;
    let pool = PgPool::connect(&url_for(&database)).await.expect("connect");

    build_base(&pool, &role).await;
    migrate(&pool, &role).await;

    // The subject, established before it is mutated. A test that only asserted
    // the red would pass against a battery that was red all along.
    assert!(
        closed_iff_terminal_present(&pool).await,
        "the step is supposed to leave the constraint on the table"
    );
    gwk_kernel::migrate::assert_protections(&pool)
        .await
        .expect("the protections hold after a migration");

    // Mutation 1: the implication. `state <> 'closed' OR closed_at IS NOT NULL`
    // still refuses a closed row with a NULL `closed_at`, so a one-directional
    // arm would call this a pass. The half it admits is the other one.
    rewrite_closed_iff_terminal(&pool, Some("state <> 'closed' OR closed_at IS NOT NULL")).await;
    assert!(
        closed_iff_terminal_present(&pool).await,
        "the mutation is a WRONG constraint, not a missing one"
    );
    let refusal = gwk_kernel::migrate::assert_protections(&pool)
        .await
        .expect_err("an implication is not an iff and must red the battery");
    let message = refusal.to_string();
    assert!(
        message.contains("a running session still carrying a closed_at"),
        "the refusal names the half the implication admits: {message}"
    );

    // Mutation 2: gone entirely. Both halves are accepted now, so the FIRST
    // probe is the refusal — which is how this arm distinguishes a constraint
    // that came back wrong from one that did not come back.
    rewrite_closed_iff_terminal(&pool, None).await;
    assert!(
        !closed_iff_terminal_present(&pool).await,
        "the DROP left the constraint behind, so the assertion below proves nothing"
    );
    let refusal = gwk_kernel::migrate::assert_protections(&pool)
        .await
        .expect_err("a dropped constraint must red the battery");
    let message = refusal.to_string();
    assert!(
        message.contains("a closed session with no closed_at"),
        "with the constraint gone the first probe is the refusal: {message}"
    );

    // And back: green again over the constraint the contract actually declares,
    // which is what says the two reds came from the mutation rather than from
    // anything this test did to get there.
    rewrite_closed_iff_terminal(&pool, Some("(state = 'closed') = (closed_at IS NOT NULL)")).await;
    assert!(
        closed_iff_terminal_present(&pool).await,
        "the restore did not put a constraint back"
    );
    gwk_kernel::migrate::assert_protections(&pool)
        .await
        .expect("the restored iff is green");

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

    // The PROPERTY first, and independently of which mechanism delivers it:
    // both spellings are refused. The assertion above reds when someone ADDS a
    // guard, which is the benign direction; these two red when someone drops
    // the foreign key or cost_entry's guard, which is the failure that matters
    // and the one the pin alone would sit silently through.
    let bare = sqlx::raw_sql("TRUNCATE gwk.dispatch_node;")
        .execute(&pool)
        .await
        .expect_err("TRUNCATE gwk.dispatch_node must be refused, by whatever refuses it");
    let cascade = sqlx::raw_sql("TRUNCATE gwk.dispatch_node CASCADE;")
        .execute(&pool)
        .await
        .expect_err("TRUNCATE ... CASCADE must be refused, by whatever refuses it");

    // Then WHICH mechanism, so the report says what is actually holding it up.
    // If either stops matching while the refusals above still hold, something
    // changed and the ADR's account of this table is stale.
    assert!(
        bare.to_string().contains("foreign key"),
        "still refused, but no longer by the foreign key the ADR says it rests on: {bare}"
    );
    assert!(
        cascade
            .to_string()
            .contains("gwk.cost_entry is append-only"),
        "still refused, but no longer by cost_entry's own guard: {cascade}"
    );

    // And it is in the set the battery's DELETE arm walks — which for a long
    // time it was not. That arm iterated the TRUNCATE set on the assumption that
    // the two are the same relations, so the one table this whole test exists to
    // describe was the one table neither arm ever named. The query is the
    // battery's own predicate over this relation alone.
    let counted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
          WHERE NOT t.tgisinternal AND t.tgtype & 1 > 0 AND t.tgtype & 8 > 0 \
            AND t.tgenabled = 'A' AND c.oid = 'gwk.dispatch_node'::regclass",
    )
    .fetch_one(&pool)
    .await
    .expect("count dispatch_node row-level delete guards");
    assert_eq!(
        counted, 1,
        "the row-level DELETE guard is how the battery counts dispatch_node at all, and it is \
         gone or no longer ALWAYS-enabled"
    );

    drop(pool);
    drop_database(&maintenance, &database).await;
    drop_role(&maintenance, &role).await;
}

/// A migration leaves no checkpoint from the contract it replaced.
///
/// Every row in `gwk_internal.checkpoint` when the applier runs was written
/// under the base contract — the verb holds the writer lock, so nothing
/// appended while it ran — and its `projection_hash` was taken over the OLD
/// `ProjectionRecord` shape. A row that survived would sit at the watermark of
/// the next restart, be compared against a hash taken over the NEW shape, and
/// make `recover()` report `Diverged` for a database that is not divergent:
/// the kernel would refuse to serve.
#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn a_migration_leaves_no_checkpoint_from_the_contract_it_replaced() {
    let maintenance = maintenance_pool().await;
    let role = role_for("discard");
    create_role(&maintenance, &role).await;
    let database = fresh_database(&maintenance, "discard").await;
    let pool = PgPool::connect(&url_for(&database)).await.expect("connect");
    build_base(&pool, &role).await;

    seed_checkpoint(&pool, 1).await;
    // BEFORE the migration, and this is the assertion that gives the two below
    // a subject. "It discarded everything" and "there was nothing to discard"
    // are the same observation over an empty table, and a fold over nothing
    // agrees with any claim made about it.
    assert_eq!(
        checkpoint_count(&pool).await,
        1,
        "the fixture did not land, so the counts below would pass over an empty table"
    );

    let applied = migrate(&pool, &role).await;

    assert_eq!(
        applied.checkpoints_discarded, 1,
        "the receipt is the only artifact that will ever say how much evidence this run dropped"
    );
    assert_eq!(
        checkpoint_count(&pool).await,
        0,
        "a checkpoint written under the replaced contract survived: the next restart would \
         compare its old-shape hash against the new shape and refuse to serve"
    );

    drop(pool);
    drop_database(&maintenance, &database).await;
    drop_role(&maintenance, &role).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn a_failing_step_moves_neither_the_fingerprint_nor_the_ledger_nor_the_checkpoints() {
    // Criterion 4. All three, because a fingerprint that held while a ledger row
    // leaked would be a receipt claiming a migration that did not happen — and
    // the ledger is append-only, so the claim would be permanent. The
    // checkpoints are here for the mirror-image reason: the discard is a real
    // deletion, and a migration that changed nothing must not take with it the
    // evidence the still-current contract's next restart is entitled to.
    //
    // TWO poisoned steps, and the second is the one the checkpoint claim rests
    // on. A step that cannot run fails in the applier's step loop, which is
    // BEFORE the discard — so the checkpoint survives it whether the discard is
    // scoped to the transaction or not, and an arm built on that poison alone
    // would be inert against exactly the mistake it names. The second step runs
    // to completion, records its ledger row, reaches the discard, and is then
    // refused by the protections rung inside the same transaction: the only
    // window in which "inside the transaction" is an observable claim.
    let maintenance = maintenance_pool().await;
    let role = role_for("failing");
    create_role(&maintenance, &role).await;
    let database = fresh_database(&maintenance, "failing").await;
    let pool = PgPool::connect(&url_for(&database)).await.expect("connect");
    build_base(&pool, &role).await;

    seed_checkpoint(&pool, 1).await;
    assert_eq!(
        checkpoint_count(&pool).await,
        1,
        "the fixture did not land, so the count after the refusal would prove nothing"
    );

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
    assert_eq!(
        checkpoint_count(&pool).await,
        1,
        "the failed transaction discarded a checkpoint anyway — and it never even reached the \
         discard, so something outside the applier deleted the row"
    );

    // The discriminating arm. This step applies cleanly, stamps the fingerprint,
    // carries its backend migrations, writes its ledger row and reaches the
    // discard — and then leaves `gwk.event`'s TRUNCATE guard disabled, which the
    // protections rung refuses INSIDE the transaction, after the checkpoints are
    // gone. A discard executed on the pool rather than on the transaction is
    // invisible to every other assertion here and fatal to this one.
    let late = Step {
        sql: Box::leak(
            format!(
                "{}\nALTER TABLE gwk.event DISABLE TRIGGER event_no_truncate;\n",
                chain[0].sql
            )
            .into_boxed_str(),
        ),
        ..*chain[0]
    };
    let refusal = gwk_kernel::migrate::apply(
        &pool,
        &[&late],
        &role,
        BASE_CONTRACT_SHA256,
        "0000000000000000000000000000000000000000",
        None,
    )
    .await
    .expect_err("a step that leaves a protection disabled");
    assert!(
        refusal.to_string().contains("TRUNCATE guard"),
        "the refusal came from somewhere other than the protections rung, so it may have \
         preceded the discard: {refusal}"
    );
    assert_eq!(
        checkpoint_count(&pool).await,
        1,
        "a step refused AFTER the discard still took the checkpoints with it: the discard is \
         running outside the applier's transaction"
    );
    assert_eq!(
        recorded_contract(&pool).await,
        BASE_CONTRACT_SHA256,
        "the fingerprint moved under a step the protections rung refused"
    );

    drop(pool);
    drop_database(&maintenance, &database).await;
    drop_role(&maintenance, &role).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see the module docs"]
async fn r5_catches_a_step_that_holds_the_event_count_and_moves_the_watermark() {
    // Criterion 2's other half. "Every event survives" is a claim the count
    // cannot make on its own: a step that deletes the newest event and inserts
    // a replacement leaves `count(*)` exactly where it was, and every arm that
    // reads only the count reports a migration that touched nothing.
    //
    // The step below is that step, written the only way a real one could be —
    // gwk.event's append-only guard is ENABLE ALWAYS, so even the superuser
    // applying a migration has to turn it off first.
    let maintenance = maintenance_pool().await;
    let role = role_for("r5watermark");
    create_role(&maintenance, &role).await;
    let database = fresh_database(&maintenance, "r5watermark").await;
    let pool = PgPool::connect(&url_for(&database)).await.expect("connect");
    build_base(&pool, &role).await;

    // Two events, so the log has a watermark to move. A migration over an
    // empty log compares None to None, which is honest and proves nothing —
    // this test is not that case, and the assertions below say so.
    seed_gate_decided_event(&pool, 1, "gate-watermark", r#"{"kind": "operator"}"#).await;
    seed_gate_decided_event(&pool, 2, "gate-watermark", r#"{"kind": "engine"}"#).await;

    let chain = resolved_chain();
    let poisoned = Step {
        sql: Box::leak(
            format!(
                "{}\n\
                 ALTER TABLE gwk.event DISABLE TRIGGER event_append_only;\n\
                 DELETE FROM gwk.event WHERE seq = 2;\n\
                 INSERT INTO gwk.event (seq, event_id, project_id, aggregate_type, \
                   aggregate_id, aggregate_version, event_type, schema_version, occurred_at, \
                   appended_at, actor, origin, payload) \
                 VALUES (3, 'evt-replacement', 'proj-test', 'gate', 'gate-watermark', 2, \
                   'gate_decided', 1, now(), now(), '{{\"kind\": \"engine\"}}'::jsonb, \
                   '{{}}'::jsonb, '{{}}'::jsonb);\n\
                 ALTER TABLE gwk.event ENABLE ALWAYS TRIGGER event_append_only;\n",
                chain[0].sql
            )
            .into_boxed_str(),
        ),
        ..*chain[0]
    };
    let applied = gwk_kernel::migrate::apply(
        &pool,
        &[&poisoned],
        &role,
        BASE_CONTRACT_SHA256,
        "0000000000000000000000000000000000000000",
        None,
    )
    .await
    .expect("the poisoned step commits — that is the point");

    // The count arm, shown to be insufficient rather than asserted to be. This
    // is the whole argument for carrying a second measurement: if this ever
    // stops holding, the watermark comparison below is no longer the thing
    // catching this step and the test has quietly changed subject.
    assert_eq!(
        applied.events_before, applied.events_after,
        "the step held the count exactly, which is what makes the count blind to it"
    );
    assert_eq!(
        applied.watermark_before,
        Some(gwk_domain::ids::Seq::new(2)),
        "the log's highest sequence before the step"
    );
    assert_eq!(
        applied.watermark_after,
        Some(gwk_domain::ids::Seq::new(3)),
        "and after — the replacement landed above the event it replaced"
    );

    let refusal = gwk_kernel::migrate::assert_result(&pool, &applied)
        .await
        .expect_err("an event removed and replaced must red R5");
    let message = refusal.to_string();
    assert!(
        message.contains("highest sequence move from 2 to 3"),
        "the refusal names the watermark, not the count: {message}"
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

/// A chain of `count` steps from `base`, each one stamping the fingerprint and
/// touching nothing else.
///
/// Synthetic because the registry holds one step and the property under test
/// only appears at several: the ledger's `step_id` is bounded at 128 characters
/// and a step id is 21 of them, so a run that wrote one row naming the whole
/// chain fitted five steps and violated the CHECK at the sixth — at the last
/// statement before the commit, after every ALTER had already run.
///
/// The SQL moves the fingerprint and adds no relation, which is what lets this
/// run on top of a real migration: R3 counts the relations of the migrated
/// schema, and a chain that changed the count would fail that rung for a reason
/// this test is not about.
fn synthetic_chain(base: &str, count: usize) -> Vec<Step> {
    let mut steps: Vec<Step> = Vec::with_capacity(count);
    let mut from: &'static str = Box::leak(base.to_owned().into_boxed_str());
    for index in 0..count {
        let digit = char::from(b'a' + u8::try_from(index).expect("fewer than 26 steps"));
        let to: &'static str = Box::leak(digit.to_string().repeat(64).into_boxed_str());
        let id: &'static str =
            Box::leak(format!("{}-{}.sql", &from[..8], &to[..8]).into_boxed_str());
        let sql: &'static str = Box::leak(
            format!(
                "UPDATE gwk_internal.schema_fingerprint SET contract_sha256 = '{to}' \
                 WHERE id = 1;\n"
            )
            .into_boxed_str(),
        );
        steps.push(Step {
            id,
            base: from,
            result: to,
            backend_migrations: &[],
            sql,
        });
        from = to;
    }
    steps
}

/// The ledger records one row per applied step, and a six-step chain fits.
///
/// `0006_schema_migration` describes itself as holding one row per applied step
/// so the sequence a database took is reconstructible. The applier wrote one row
/// per RUN, with `step_id` set to the chain joined by ", ", and that is a
/// different record with two faults. It overflows the column's CHECK at six
/// steps — 21 characters each plus separators is 136 against a bound of 128 —
/// and it spends the column on a value that is not a step id, when matching it
/// against a file under `schema/steps/` is what a later reader is promised.
///
/// Six is the count deliberately: five fitted, so a chain of five would pass
/// against both spellings and prove nothing.
#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_ledger_writes_one_row_per_step_and_a_six_step_chain_fits() {
    const BACKUP: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

    let maintenance = maintenance_pool().await;
    let role = role_for("ledger");
    create_role(&maintenance, &role).await;
    let database = fresh_database(&maintenance, "ledger").await;
    let pool = PgPool::connect(&url_for(&database)).await.expect("connect");

    build_base(&pool, &role).await;
    migrate(&pool, &role).await;

    // On top of the migrated schema, so the rungs inside the applier are asked
    // of the shape they were written for.
    let chain = synthetic_chain(CONTRACT_SQL_SHA256, 6);
    let borrowed: Vec<&Step> = chain.iter().collect();
    let applied = gwk_kernel::migrate::apply(
        &pool,
        &borrowed,
        &role,
        CONTRACT_SQL_SHA256,
        "0000000000000000000000000000000000000000",
        Some(BACKUP),
    )
    .await
    .expect("a six-step chain applies");
    assert_eq!(applied.steps.len(), 6, "{:?}", applied.steps);

    // COUNT first. Indexing into a ledger of the wrong length panics about the
    // wrong thing, and a query that returned nothing would make every per-row
    // comparison below vacuously true.
    let rows = sqlx::query(
        "SELECT step_id, base_sha256, result_sha256, backup_sha256 \
           FROM gwk_internal.schema_migration ORDER BY seq",
    )
    .fetch_all(&pool)
    .await
    .expect("read the ledger");
    assert_eq!(
        rows.len(),
        1 + chain.len(),
        "the real migration's row plus one per synthetic step"
    );

    for (step, row) in chain.iter().zip(rows.iter().skip(1)) {
        let step_id: String = row.get("step_id");
        let base: String = row.get("base_sha256");
        let result: String = row.get("result_sha256");
        let backup: Option<String> = row.get("backup_sha256");
        assert_eq!(step_id, step.id);
        assert_eq!(base, step.base);
        assert_eq!(result, step.result);
        // One backup was taken before the one transaction all six ran in, so it
        // is equally the restore point for each of them.
        assert_eq!(backup.as_deref(), Some(BACKUP));
        // The shape that overflowed, pinned directly rather than inferred from
        // the fact that this run happened to fit.
        assert!(
            !step_id.contains(", "),
            "a ledger row names more than one step: {step_id:?}"
        );
    }

    drop(pool);
    drop_database(&maintenance, &database).await;
    drop_role(&maintenance, &role).await;
}
