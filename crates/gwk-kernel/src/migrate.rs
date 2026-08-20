//! Resolving the migration chain between the contract digest a database
//! records and the one this binary carries.
//!
//! `gwk_internal.schema_fingerprint` holds one digest,
//! [`CONTRACT_SQL_SHA256`](crate::CONTRACT_SQL_SHA256) is another, and when they
//! differ the question is whether this build knows a path from the first to the
//! second. That path is a LINE, not a graph: each
//! step names exactly the digest it may be applied to and exactly the digest it
//! produces, so at most one step can follow any other. Everything here exists
//! to keep it that way, because the alternative is a solver — and a solver is a
//! component whose bugs are silent, which is the last thing a schema migration
//! should be.
//!
//! Nothing in this module touches a database. It is handed a registry and two
//! digests and answers with a chain or a refusal; applying the chain is
//! somebody else's act, and that separation is what makes the decision table
//! testable without a server — the same split [`crate::admin::classify`] takes.
//!
//! The registry itself is generated: `schema/steps/*.sql` are the authored
//! files, `xtask/src/steps.rs` reads them, and it is that generator — not this
//! resolver — that holds a step's file name to the digests in its header.

use sqlx::Row as _;

use gwk_domain::contract_steps::Step;
use gwk_domain::ids::Seq;
use gwk_domain::is_sha256_hex;

/// [`crate::recover::watermark_of`], in this module's error type.
///
/// The same read recovery does, deliberately, rather than a second
/// `SELECT max(seq)`: a watermark the migrate path invented could agree with
/// itself while meaning something other than the position recovery anchors on,
/// and there would be nothing to notice it. Only the error type has to be
/// translated — recovery answers in wire `Refusal`s and this module in
/// [`crate::KernelError`].
async fn watermark(conn: &mut sqlx::PgConnection) -> crate::Result<Option<Seq>> {
    crate::recover::watermark_of(conn)
        .await
        .map_err(|refusal| crate::KernelError::Schema(refusal.to_string()))
}

/// Why a chain could not be produced.
///
/// Every variant carries its evidence as data rather than as a formatted
/// string. The rendering is one `Display` impl below, and a caller that wants
/// to count what a refusal found — how many bases a registry actually held, say
/// — can, instead of parsing the sentence back out of a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainRefusal {
    /// The registry is empty. Distinct from [`ChainRefusal::NoChain`] on
    /// purpose: "no step bases on your digest" is what a populated registry
    /// says, and reporting it for a registry that was never populated is the
    /// wrong diagnosis of the right symptom.
    NoSteps {
        /// The digest the database records.
        from: String,
        /// The digest this binary carries.
        to: String,
    },

    /// A step carries something that is not a 64-character lowercase hex
    /// digest, so nothing downstream can compare it byte for byte.
    Malformed {
        /// The offending step's id.
        id: String,
        /// Which of the step's two digests it was.
        field: &'static str,
        /// What the field held.
        digest: String,
    },

    /// A step's base and result are the same digest, which is a one-element
    /// loop wearing a step's clothes.
    SelfStep {
        /// The offending step's id.
        id: String,
        /// The digest on both sides.
        digest: String,
    },

    /// Two steps base on one digest. Refused when the registry is read, not
    /// when a chain is walked: the chain being a line is a property of the
    /// registry, and a build that ships a fork should not depend on somebody
    /// asking for the route that crosses it.
    Branching {
        /// The digest both steps claim.
        base: String,
        /// The ids of the two steps that claim it, in registry order.
        ids: Vec<String>,
    },

    /// The walk arrived somewhere it had already been.
    Cycle {
        /// The digest reached for the second time.
        digest: String,
    },

    /// The walk ran out of steps before reaching the target.
    NoChain {
        /// The digest the database records.
        from: String,
        /// The digest this binary carries.
        to: String,
        /// Where the walk stopped. Equal to `from` when nothing bases on the
        /// recorded digest at all, and some later digest when a chain exists
        /// but arrives somewhere else.
        reached: String,
        /// Every base in the registry, in registry order. Carried rather than
        /// formatted so a caller can count them — a diagnostic that names one
        /// candidate is the diagnostic that sends a reader looking for the
        /// other twelve.
        bases: Vec<String>,
    },
}

impl std::fmt::Display for ChainRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSteps { from, to } => write!(
                f,
                "no steps are registered: this build carries an empty migration registry, so it \
                 knows of no way to carry a database from {from} to {to}"
            ),
            Self::Malformed { id, field, digest } => write!(
                f,
                "step {id}: {field} is {digest:?}, which is not a 64-character lowercase hex \
                 digest and cannot be compared against one"
            ),
            Self::SelfStep { id, digest } => write!(
                f,
                "step {id}: base and result are both {digest} — a step that arrives where it \
                 started can only be applied forever"
            ),
            Self::Branching { base, ids } => write!(
                f,
                "steps {} both base on {base}: the migration chain is a line, and two ways \
                 forward from one digest would make choosing between them a solver",
                ids.join(" and ")
            ),
            Self::Cycle { digest } => write!(
                f,
                "the chain revisits {digest}: migration steps form a line, not a loop"
            ),
            Self::NoChain {
                from,
                to,
                reached,
                bases,
            } if reached == from => write!(
                f,
                "no step bases on {from}, the contract digest this database records; this build \
                 carries {to}. Known bases: {}",
                bases.join(", ")
            ),
            Self::NoChain {
                from,
                to,
                reached,
                bases,
            } => write!(
                f,
                "the chain from {from} ends at {reached}, and this build carries {to} — the last \
                 step of the chain is not the one that arrives. Known bases: {}",
                bases.join(", ")
            ),
        }
    }
}

impl std::error::Error for ChainRefusal {}

/// The steps that carry a database from `from` to `to`, in application order.
///
/// `from` is the digest a database records and `to` the digest this binary
/// carries; the caller has already established that they differ (an equal pair
/// is [`crate::admin::InitOutcome::AlreadyInitialized`], answered before
/// anything reaches here).
///
/// The registry is validated as a whole before any of it is walked, so a
/// refusal is a statement about this build rather than about the route the
/// caller happened to ask for.
pub fn resolve<'a>(
    steps: &'a [Step],
    from: &str,
    to: &str,
) -> std::result::Result<Vec<&'a Step>, ChainRefusal> {
    // The count decides, before anything folds over the registry. A walk across
    // zero steps terminates immediately and reports that nothing bases on the
    // recorded digest, which is true and useless: the registry it inspected was
    // never populated. `all()` over an empty set is true and a search over an
    // empty set finds nothing, and neither observation distinguishes the two
    // states this has to tell apart.
    let registered = steps.len();
    if registered == 0 {
        return Err(ChainRefusal::NoSteps {
            from: from.to_owned(),
            to: to.to_owned(),
        });
    }

    validate(steps)?;
    walk(steps, from, to)
}

/// Check the registry as a whole, independent of any route through it.
///
/// Everything here is a property of what this build ships, so it is settled
/// once, up front — a fork or a malformed digest in a corner of the registry
/// nobody asked about is still a build that should not migrate anything.
fn validate(steps: &[Step]) -> std::result::Result<(), ChainRefusal> {
    // (base, id) for every step already inspected. A `Vec` scan rather than a
    // map: the registry is one file per schema change, so it is a handful of
    // entries, and the linear scan keeps the FIRST claimant of a base — which
    // is what the refusal names.
    let mut claimed: Vec<(&str, &str)> = Vec::with_capacity(steps.len());

    for step in steps {
        for (field, digest) in [("base", step.base), ("result", step.result)] {
            if !is_sha256_hex(digest) {
                return Err(ChainRefusal::Malformed {
                    id: step.id.to_owned(),
                    field,
                    digest: digest.to_owned(),
                });
            }
        }
        if step.base == step.result {
            return Err(ChainRefusal::SelfStep {
                id: step.id.to_owned(),
                digest: step.base.to_owned(),
            });
        }
        if let Some((_, first)) = claimed.iter().find(|(base, _)| *base == step.base) {
            return Err(ChainRefusal::Branching {
                base: step.base.to_owned(),
                ids: vec![(*first).to_owned(), step.id.to_owned()],
            });
        }
        claimed.push((step.base, step.id));
    }

    Ok(())
}

/// Follow the one step out of each digest until `to` is reached, or until it is
/// clear that it will not be.
///
/// [`validate`] has already established that no digest has two steps out of it,
/// so there is never a choice to make here — which is the whole reason this is a
/// walk and not a search.
fn walk<'a>(
    steps: &'a [Step],
    from: &str,
    to: &str,
) -> std::result::Result<Vec<&'a Step>, ChainRefusal> {
    let mut chain: Vec<&Step> = Vec::new();
    let mut visited: Vec<&str> = vec![from];
    let mut cursor = from;

    while cursor != to {
        let Some(step) = steps.iter().find(|step| step.base == cursor) else {
            return Err(ChainRefusal::NoChain {
                from: from.to_owned(),
                to: to.to_owned(),
                reached: cursor.to_owned(),
                bases: steps.iter().map(|step| step.base.to_owned()).collect(),
            });
        };
        // Unique bases make the graph a function, so a revisit is a loop and a
        // loop never ends. Bounding the walk by the step count would catch it
        // too, but only as "this took too long" — and the operator needs the
        // digest to know which pair of files to look at.
        if visited.contains(&step.result) {
            return Err(ChainRefusal::Cycle {
                digest: step.result.to_owned(),
            });
        }
        visited.push(step.result);
        chain.push(step);
        cursor = step.result;
    }

    Ok(chain)
}

/// What one applied chain did, for the receipt the operator reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// The digest the database recorded before the chain ran.
    pub base: String,
    /// The digest it records after.
    pub result: String,
    /// The step ids applied, in order.
    pub steps: Vec<String>,
    /// The backend migration stems applied, in order.
    pub backend_migrations: Vec<String>,
    /// Events in the log before the chain ran.
    pub events_before: i64,
    /// Events in the log after. The chain writes none, so a difference here is
    /// a daemon that was not fenced.
    pub events_after: i64,
    /// The log's highest assigned sequence before the chain ran, or `None` on
    /// an empty log.
    pub watermark_before: Option<Seq>,
    /// And after.
    ///
    /// Carried BESIDE the count rather than instead of it, because neither one
    /// covers the other. A step that deleted the newest event and inserted a
    /// replacement holds the count exactly and moves this; a step that
    /// renumbered the log holds this exactly and moves the count. Both are what
    /// "every event survives" is a claim against.
    pub watermark_after: Option<Seq>,
    /// How long the one transaction took, in milliseconds.
    pub elapsed_ms: u64,
}

/// Apply `chain` to `pool` in ONE transaction.
///
/// Order, and each part is here because the one before it made it possible:
/// the step DDL, then the backend migrations the steps declare, then the
/// privilege matrix wholesale, then the ledger row. Privileges come after the
/// migrations because `GRANT ... ON ALL TABLES IN SCHEMA gwk` reaches the
/// relations that exist when it runs, and the ledger row comes last because
/// `0006_schema_migration` may be one of the migrations that just arrived.
///
/// One transaction, so a crash leaves the database at its base fingerprint —
/// which the binaries that were serving it a moment ago still serve. A
/// half-applied contract matches no digest at all and there is nothing to
/// compare it against.
///
pub async fn apply(
    pool: &sqlx::PgPool,
    chain: &[&Step],
    role: &str,
    recorded_base: &str,
    public_revision: &str,
    backup_sha256: Option<&str>,
) -> crate::Result<Applied> {
    // Counted before anything runs: an empty chain would commit a transaction
    // that changed nothing and return a receipt saying a migration happened.
    if chain.is_empty() {
        return Err(crate::KernelError::Schema(
            "refusing to apply an empty chain: there is no migration to record".to_owned(),
        ));
    }

    let started = std::time::Instant::now();
    let mut tx = pool.begin().await?;

    // Every `ALTER TABLE` below needs ACCESS EXCLUSIVE, and the writer lock this
    // verb holds is a `pg_try_advisory_lock` that excludes another kernel writer
    // and nothing else. A psql session, a dashboard read, a `pg_dump` — none of
    // them take it, and any one of them holding ACCESS SHARE makes the first
    // ALTER wait forever. PostgreSQL's lock queue is FIFO, so while it waits it
    // also queues every later reader behind it: an operator watching a hung
    // migrate would be watching it wedge the table it is migrating. A refusal is
    // recoverable and legible; an unbounded wait is neither. `SET LOCAL` scopes
    // this to the transaction, so nothing outside it inherits the deadline.
    sqlx::raw_sql(sqlx::AssertSqlSafe(
        "SET LOCAL lock_timeout = '5s';".to_owned(),
    ))
    .execute(&mut *tx)
    .await?;

    let events_before: i64 = sqlx::query_scalar("SELECT count(*) FROM gwk.event")
        .fetch_one(&mut *tx)
        .await?;
    let watermark_before = watermark(&mut tx).await?;

    let mut applied_steps: Vec<String> = Vec::with_capacity(chain.len());
    let mut applied_migrations: Vec<String> = Vec::new();

    for step in chain {
        sqlx::raw_sql(sqlx::AssertSqlSafe(step.sql))
            .execute(&mut *tx)
            .await?;
        applied_steps.push(step.id.to_owned());

        for stem in step.backend_migrations {
            // Resolved from the kernel's own `BACKEND_MIGRATIONS`, so the file
            // a step names and the file that runs are the same bytes the
            // initializer would have run. A name with no migration is refused
            // here rather than becoming a missing relation later.
            let sql = crate::admin::backend_migration(stem).ok_or_else(|| {
                crate::KernelError::Schema(format!(
                    "{} carries backend migration {stem:?}, which this binary does not have",
                    step.id
                ))
            })?;
            sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
                .execute(&mut *tx)
                .await?;
            applied_migrations.push((*stem).to_owned());
        }
    }

    // Wholesale, never a delta. The grant surface is a function of the table
    // set, and the step just changed the table set.
    //
    // The audit `AssertSqlSafe` demands: the only runtime-substituted value is
    // `role`, restricted to `[a-z_][a-z0-9_]*` by `config::validate_role`, so
    // it can carry no quote, separator, or comment.
    sqlx::raw_sql(sqlx::AssertSqlSafe(crate::admin::privilege_statements(
        role,
    )))
    .execute(&mut *tx)
    .await?;

    let result = chain[chain.len() - 1].result;

    // The fingerprint is asserted, not written. Each step stamps it as its last
    // act, so reading it back here is a check on the chain rather than on this
    // function's own memory of what it meant to do.
    let recorded: String = sqlx::query_scalar(
        "SELECT contract_sha256 FROM gwk_internal.schema_fingerprint WHERE id = 1",
    )
    .fetch_one(&mut *tx)
    .await?;
    if recorded != result {
        return Err(crate::KernelError::Schema(format!(
            "the chain ended at {result} and the database records {recorded}: the last step did \
             not stamp gwk_internal.schema_fingerprint, so nothing rolled back would have been \
             distinguishable from nothing applied"
        )));
    }

    sqlx::query(
        "INSERT INTO gwk_internal.schema_migration \
           (base_sha256, result_sha256, step_id, public_revision, backup_sha256, \
            backend_migrations) \
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(recorded_base)
    .bind(result)
    .bind(applied_steps.join(", "))
    .bind(public_revision)
    .bind(backup_sha256)
    .bind(&applied_migrations)
    .execute(&mut *tx)
    .await?;

    let events_after: i64 = sqlx::query_scalar("SELECT count(*) FROM gwk.event")
        .fetch_one(&mut *tx)
        .await?;
    let watermark_after = watermark(&mut tx).await?;

    // R3 and R4, INSIDE the transaction, which is the only place they can
    // refuse rather than merely report. Both are questions about the schema the
    // step just produced, and the catalogue changes are visible here — so asking
    // them after the commit, as an earlier draft did, bought nothing and cost
    // the ability to roll back. A step that widens the grant matrix or leaves an
    // append-only guard broken now aborts this transaction; before, it committed
    // and was told about afterwards, with no exit but the operator's own dump.
    //
    // R5 stays after the commit deliberately. It re-reads the log to catch a
    // writer that was never fenced, and a measurement taken inside the
    // transaction that would have to detect it cannot see anything outside.
    assert_grant_matrix(&mut *tx, role).await?;
    assert_protections_within(&mut tx).await?;

    tx.commit().await?;

    Ok(Applied {
        base: recorded_base.to_owned(),
        result: result.to_owned(),
        steps: applied_steps,
        backend_migrations: applied_migrations,
        events_before,
        events_after,
        watermark_before,
        watermark_after,
        elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}

/// R1 — the database records the digest the chain starts from.
///
/// Before any statement executes. A chain resolved from a base the database is
/// not at would apply DDL to a shape it does not describe, and the first
/// statement that happened to succeed would leave a half-migrated database
/// matching no digest at all.
///
/// A database already at the target is refused here rather than deeper: the
/// resolver answers an equal pair with an empty chain, and "refusing to apply
/// an empty chain" is a true sentence about the wrong subject.
pub async fn assert_base(pool: &sqlx::PgPool, expected: &str) -> crate::Result<()> {
    let recorded: Option<String> = sqlx::query_scalar(
        "SELECT contract_sha256 FROM gwk_internal.schema_fingerprint WHERE id = 1",
    )
    .fetch_optional(pool)
    .await?;
    let Some(recorded) = recorded else {
        return Err(crate::KernelError::Schema(
            "gwk_internal.schema_fingerprint has no row: this database has never been \
             initialized by the kernel, and a migration carries a database that already records \
             a contract"
                .to_owned(),
        ));
    };
    if recorded == crate::CONTRACT_SQL_SHA256 {
        return Err(crate::KernelError::Schema(format!(
            "this database already carries {recorded}, the contract this binary carries: there \
             is nothing to migrate"
        )));
    }
    if recorded != expected {
        return Err(crate::KernelError::Schema(format!(
            "this database records {recorded} and the chain starts from {expected}"
        )));
    }
    Ok(())
}

/// Whether an error is THIS relation's own append-only guard refusing.
///
/// Two conditions, and the arms below are wrong without either. SQLSTATE
/// `P0001` is `raise_exception`, which only a PL/pgSQL `RAISE` produces: every
/// guard in both schemas is one, and no foreign key, privilege check or lock
/// timeout can forge it. And the message has to name this relation, because
/// under CASCADE another relation's guard answers with a `P0001` of its own —
/// probing a table whose guard has been dropped is met by a referencing table's
/// guard, which is a true refusal about the wrong subject.
///
/// The trailing space is not incidental. Guard messages carry the relation
/// either bare (`task rows cannot be deleted`) or schema-qualified
/// (`gwk.event is append-only (TRUNCATE refused)`), so testing for the bare name
/// matches both shapes — but `pty_session` is a prefix of
/// `pty_session_template`, and without a boundary the template's refusal would
/// be credited to the session's guard.
fn is_guard_refusal(error: &sqlx::Error, relation: &str) -> bool {
    let sqlx::Error::Database(ref database) = *error else {
        return false;
    };
    database.code().as_deref() == Some("P0001")
        && database.message().contains(&format!("{relation} "))
}

/// R4 — the protections refuse a superuser, not merely an ungranted role.
///
/// As superuser deliberately: a grant binds the runtime role and nothing else,
/// and the credential applying a migration is the admin one. A battery that
/// stays green with `ALTER TABLE gwk.event DISABLE TRIGGER event_append_only`
/// is testing the grants, and it has no subject.
///
/// Each attempt runs inside its own SAVEPOINT, so a refusal does not poison the
/// transaction the caller is in the middle of and a statement that WRONGLY
/// succeeds is rolled back rather than left behind.
pub async fn assert_protections(pool: &sqlx::PgPool) -> crate::Result<()> {
    let mut tx = pool.begin().await?;
    let outcome = assert_protections_within(&mut tx).await;
    // Explicitly, not by drop: the probes below TRUNCATE and DELETE real
    // relations, and "the transaction was never committed" is a thing worth
    // seeing in the code rather than inferring from a destructor.
    tx.rollback().await?;
    outcome
}

/// [`assert_protections`], inside a transaction the caller already holds.
///
/// The applier runs the rung here rather than after its commit, which is the
/// difference between refusing a migration that breaks an append-only guard and
/// committing one and then reporting it. Everything the probes do is undone
/// through a savepoint before this returns, so the caller's transaction is left
/// exactly as it was found — a migration is not rolled back by proving itself.
pub(crate) async fn assert_protections_within(
    tx: &mut sqlx::PgTransaction<'_>,
) -> crate::Result<()> {
    sqlx::raw_sql(sqlx::AssertSqlSafe("SAVEPOINT protections;".to_owned()))
        .execute(&mut **tx)
        .await?;

    let outcome = assert_protections_probes(tx).await;

    // Only on the way out clean. A refusal aborts the caller's whole
    // transaction anyway, and reaching for the savepoint on an already-poisoned
    // connection would replace the real refusal with a driver error.
    if outcome.is_ok() {
        sqlx::raw_sql(sqlx::AssertSqlSafe(
            "ROLLBACK TO SAVEPOINT protections;".to_owned(),
        ))
        .execute(&mut **tx)
        .await?;
    }
    outcome
}

async fn assert_protections_probes(tx: &mut sqlx::PgTransaction<'_>) -> crate::Result<()> {
    // Every relation carrying a statement-level TRUNCATE guard, asked for by
    // name rather than assumed: TRUNCATE needs no rows to be refused, so this
    // arm covers every table it names without seeding one.
    //
    // `tgenabled = 'A'` is load-bearing and was missing. `pg_trigger` keeps the
    // row when a trigger is disabled — `ALTER TABLE .. DISABLE TRIGGER` sets
    // `tgenabled` to 'D' and changes nothing else — so a count over an
    // unfiltered sweep cannot see a disabled guard at all. Worse, a guard
    // created without `ENABLE ALWAYS` sits at the default 'O', and an ORIGIN
    // trigger does not fire when the session is a replica, which is precisely
    // the session a restore runs in. Both states are a guard that is present
    // in the catalogue and absent in the moment it is needed. Filtering to 'A'
    // is what makes the count below refuse a DROP, a DISABLE, and an
    // ALWAYS-to-ORIGIN downgrade with one predicate and no second query.
    let guarded: Vec<(String, String)> = sqlx::query(
        "SELECT n.nspname AS schema, c.relname AS relation \
           FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
           JOIN pg_namespace n ON n.oid = c.relnamespace \
          WHERE NOT t.tgisinternal AND t.tgtype & 32 > 0 \
            AND t.tgenabled = 'A' \
            AND n.nspname IN ('gwk', 'gwk_internal') \
          GROUP BY 1, 2 ORDER BY 1, 2",
    )
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .map(|row| (row.get("schema"), row.get("relation")))
    .collect();

    // Counted before it is walked, and with the `tgenabled` filter above this
    // is now the arm that catches every way a guard stops guarding: dropped,
    // disabled, or downgraded out of ALWAYS. The probe below iterates this same
    // set, so a relation missing from it is a relation nothing asks about, and
    // a query that returned nothing would make every refusal below vacuous in
    // the same way — the battery would report a protected database it never
    // touched.
    if guarded.len() != EXPECTED_TRUNCATE_GUARDS {
        return Err(crate::KernelError::Schema(format!(
            "expected exactly {EXPECTED_TRUNCATE_GUARDS} relations with a TRUNCATE guard and \
             found {}: {guarded:?}",
            guarded.len()
        )));
    }

    for (schema, relation) in &guarded {
        sqlx::raw_sql(sqlx::AssertSqlSafe("SAVEPOINT probe;".to_owned()))
            .execute(&mut **tx)
            .await?;
        // The identifiers come from `pg_class`, not from input.
        //
        // CASCADE, and it is not a convenience. PostgreSQL checks a table's
        // inbound foreign keys BEFORE it fires any BEFORE TRUNCATE trigger, so
        // a bare TRUNCATE of a referenced table is refused with 0A000 and the
        // guard never runs. Three of the relations here are referenced —
        // `gwk.attempt`, `gwk.task`, `gwk.context_manifest` — which means a
        // bare probe never once exercised their guards, and could not tell a
        // present guard from a dropped one, in every run. CASCADE pulls the
        // referencing tables into the same statement, the FK objection goes
        // away, and the guard is reached. Nothing is truncated either way: the
        // savepoint below is rolled back.
        let outcome = sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "TRUNCATE {schema}.{relation} CASCADE;"
        )))
        .execute(&mut **tx)
        .await;
        sqlx::raw_sql(sqlx::AssertSqlSafe(
            "ROLLBACK TO SAVEPOINT probe;".to_owned(),
        ))
        .execute(&mut **tx)
        .await?;
        match outcome {
            Ok(_) => {
                return Err(crate::KernelError::Schema(format!(
                    "TRUNCATE {schema}.{relation} succeeded: its statement-level guard is gone, \
                     and a row-level DELETE guard never fires on TRUNCATE"
                )));
            }
            // The guard, and this relation's guard specifically.
            Err(error) if is_guard_refusal(&error, relation) => {}
            Err(error) => {
                return Err(crate::KernelError::Schema(format!(
                    "TRUNCATE {schema}.{relation} was refused, but not by its own guard \
                     ({error}). A foreign key, a missing privilege and a lock timeout each \
                     refuse this probe exactly as flatly as the guard does, and under CASCADE so \
                     does another relation's guard — accepting any of them as proof is how a \
                     relation whose guard is gone reads as protected"
                )));
            }
        }
    }

    // The DELETE arm needs rows, and only rows already present can supply them
    // without a fixture that is itself untested. `gwk.transition` is seeded by
    // the contract and never empty; the ledger has a row by the time a
    // migration reaches here.
    //
    // Counted for the same reason as above, and this one is the sharper trap:
    // `DELETE FROM t` over an empty table affects no rows, fires no row-level
    // trigger, and succeeds. A DELETE battery over an empty database is green
    // and means nothing.
    let mut deleted_from = 0usize;
    for (schema, relation) in &guarded {
        // Identifiers out of `pg_class`, never out of input — the same audit
        // note the TRUNCATE above carries.
        let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT count(*) FROM {schema}.{relation}"
        )))
        .fetch_one(&mut **tx)
        .await?;
        if count == 0 {
            continue;
        }
        sqlx::raw_sql(sqlx::AssertSqlSafe("SAVEPOINT probe;".to_owned()))
            .execute(&mut **tx)
            .await?;
        let outcome = sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {schema}.{relation};"
        )))
        .execute(&mut **tx)
        .await;
        sqlx::raw_sql(sqlx::AssertSqlSafe(
            "ROLLBACK TO SAVEPOINT probe;".to_owned(),
        ))
        .execute(&mut **tx)
        .await?;
        match outcome {
            Ok(_) => {
                return Err(crate::KernelError::Schema(format!(
                    "DELETE FROM {schema}.{relation} removed {count} row(s): its row-level guard \
                     is gone"
                )));
            }
            Err(error) if is_guard_refusal(&error, relation) => {}
            Err(error) => {
                // The row-level guard is a BEFORE trigger and a foreign key's
                // is an AFTER one, so a present guard always answers first and
                // this arm is not confounded the way the TRUNCATE arm was. It
                // becomes confounded the moment the guard is gone: the FK then
                // refuses in its place, and a bare `is_err()` would read that
                // as the guard holding.
                return Err(crate::KernelError::Schema(format!(
                    "DELETE FROM {schema}.{relation} was refused, but not by its own guard \
                     ({error}): a foreign key refuses a delete just as flatly, and it is what \
                     answers once the guard is gone"
                )));
            }
        }
        deleted_from += 1;
    }
    if deleted_from == 0 {
        return Err(crate::KernelError::Schema(
            "no guarded relation held a row, so the DELETE arm proved nothing: an empty table \
             fires no row-level trigger and reports success"
                .to_owned(),
        ));
    }

    assert_no_half_closed_update(tx).await?;

    Ok(())
}

/// The half-closed UPDATE arm of [`assert_protections`].
///
/// `gwk.pty_session_closed_iff_terminal` is an IFF — closed exactly when
/// `closed_at` is set — and the step in `schema/steps/` drops it and re-adds it
/// in place. That is the one shape a battery run after the step exists to
/// notice: a constraint re-added as the one-way implication
/// `state <> 'closed' OR closed_at IS NOT NULL` compiles, validates against
/// every row a live table holds, and silently stops refusing half of what its
/// name claims.
///
/// A CHECK fires only against a row being written, so unlike the TRUNCATE arm
/// this one cannot be built by probing an empty table — it SEEDS one, inside the
/// transaction the caller rolls back. Runs after the DELETE arm so the seeded
/// row is not in the set that arm counts.
async fn assert_no_half_closed_update(tx: &mut sqlx::PgTransaction<'_>) -> crate::Result<()> {
    const PROBE_ID: &str = "gwk-migrate-protections-probe";

    sqlx::query("INSERT INTO gwk.pty_session (id, generation) VALUES ($1, 'protections-probe')")
        .bind(PROBE_ID)
        .execute(&mut **tx)
        .await?;

    // The control, and it is the whole reason the two refusals below mean
    // anything. `pty_session_cas` is a BEFORE UPDATE ROW trigger, so it fires
    // AHEAD of any CHECK: an UPDATE that forgot to advance `version` is refused
    // by the CAS guard and is indistinguishable, from here, from an UPDATE the
    // constraint refused. And an UPDATE whose WHERE matches nothing touches no
    // row, evaluates no CHECK, and succeeds — the same "summed over nothing"
    // the count arms above exist to refuse. A legal transition has to be
    // watched to land, on exactly one row, before a refusal is evidence of
    // anything.
    //
    // It also leaves the probe row closed, which is what makes each violation
    // below a single-column edit away from a legal state.
    let advanced = sqlx::query(
        "UPDATE gwk.pty_session SET version = version + 1, state = 'closed', closed_at = now() \
         WHERE id = $1",
    )
    .bind(PROBE_ID)
    .execute(&mut **tx)
    .await
    .map_err(|err| {
        crate::KernelError::Schema(format!(
            "the LEGAL half of gwk.pty_session's closed-iff-terminal constraint was refused \
             ({err}): whatever refused it would refuse the two violations below too, so this \
             arm would have reported a protected database it never probed"
        ))
    })?;
    if advanced.rows_affected() != 1 {
        return Err(crate::KernelError::Schema(format!(
            "the probe row was not there to update: {} rows affected, not 1. An UPDATE that \
             matches nothing fires no trigger and evaluates no CHECK, and reports success",
            advanced.rows_affected()
        )));
    }

    // Both directions, because they are not the same test. The one-way
    // implication above still refuses a closed row with no `closed_at` — it is
    // the OTHER half, a `running` row carrying one, that it quietly admits. An
    // arm that probed one direction would pass against exactly the drift this
    // exists to catch.
    for (violation, sql) in [
        (
            "a closed session with no closed_at",
            "UPDATE gwk.pty_session SET version = version + 1, closed_at = NULL WHERE id = $1",
        ),
        (
            "a running session still carrying a closed_at",
            "UPDATE gwk.pty_session SET version = version + 1, state = 'running' WHERE id = $1",
        ),
    ] {
        sqlx::raw_sql(sqlx::AssertSqlSafe("SAVEPOINT probe;".to_owned()))
            .execute(&mut **tx)
            .await?;
        let outcome = sqlx::query(sql).bind(PROBE_ID).execute(&mut **tx).await;
        sqlx::raw_sql(sqlx::AssertSqlSafe(
            "ROLLBACK TO SAVEPOINT probe;".to_owned(),
        ))
        .execute(&mut **tx)
        .await?;
        if outcome.is_ok() {
            return Err(crate::KernelError::Schema(format!(
                "gwk.pty_session accepted {violation}: pty_session_closed_iff_terminal is not \
                 the iff it is named for, and the step that drops and re-adds it is where that \
                 happens"
            )));
        }
    }

    Ok(())
}

/// How many relations [`assert_protections`]'s TRUNCATE sweep must find.
///
/// Seventeen are the contract's, in `schema/0001_contract.sql`; the eighteenth
/// is the ledger's own, in `migrations/0006_schema_migration.sql`.
///
/// EQUALITY, and asserted the same way [`EXPECTED_RELATIONS`] is. An earlier
/// draft made it a floor, reasoning that a relation GAINING a guard is always
/// an improvement and should not red a migration. That reasoning is wrong, and
/// the floor it produced was the whole of the hole: it left exactly one guard
/// of slack, so dropping any single guard still cleared it — and a DROPPED
/// guard is then absent from `pg_trigger`, so it is absent from `guarded`, so
/// the per-relation probe below never asks about it. The battery returned
/// `Ok(())` for a database a superuser could TRUNCATE to zero rows.
///
/// A guard also cannot arrive unannounced. Adding one edits
/// `schema/0001_contract.sql`, which moves [`crate::CONTRACT_SQL_SHA256`],
/// which the digest gate then correctly refuses until a step results in it —
/// and that step is the change where this number moves with it. There is no
/// benign direction for this count to drift in.
const EXPECTED_TRUNCATE_GUARDS: usize = 18;

/// R5 — the database ends where the chain said it would, measured rather than
/// inferred.
///
/// The event count AND the watermark are read again and compared to what was
/// read before the transaction. Inferring "the log is untouched" from the
/// absence of an error is the same mistake as inferring a passing test from a
/// suite that ran no cases.
///
/// Two measurements because one of them is not a claim about survival. A step
/// that deleted the newest event and inserted a replacement leaves the count
/// exactly where it was and moves the watermark; a step that renumbered leaves
/// the watermark and moves the count. Each is invisible to the other, and both
/// are what criterion 2 refuses.
pub async fn assert_result(pool: &sqlx::PgPool, applied: &Applied) -> crate::Result<()> {
    let recorded: String = sqlx::query_scalar(
        "SELECT contract_sha256 FROM gwk_internal.schema_fingerprint WHERE id = 1",
    )
    .fetch_one(pool)
    .await?;
    if recorded != crate::CONTRACT_SQL_SHA256 {
        return Err(crate::KernelError::Schema(format!(
            "the migration committed and this database records {recorded}, not \
             {} — it is at a contract no binary claims",
            crate::CONTRACT_SQL_SHA256
        )));
    }
    let events_now: i64 = sqlx::query_scalar("SELECT count(*) FROM gwk.event")
        .fetch_one(pool)
        .await?;
    if events_now != applied.events_after {
        return Err(crate::KernelError::Schema(format!(
            "the log held {} events inside the migration and holds {events_now} after it: \
             something else is writing, and the writer lock did not hold",
            applied.events_after
        )));
    }
    if applied.events_before != applied.events_after {
        return Err(crate::KernelError::Schema(format!(
            "the migration transaction saw the log move from {} to {}: a migration writes no \
             events, so a daemon was not fenced",
            applied.events_before, applied.events_after
        )));
    }

    // The watermark half of the same criterion, and the count above is what
    // says it was measured over something. `max(seq)` over an empty table is
    // NULL and two NULLs compare equal — honest for a log with no events, and
    // silent for every other reason the read could come back empty. A
    // non-empty log whose watermark is absent is a read that did not land
    // where it was pointed, not a log that survived.
    if applied.events_after > 0 && applied.watermark_after.is_none() {
        return Err(crate::KernelError::Schema(format!(
            "the log holds {} events and reports no watermark: the two are read from the same \
             table, so comparing the watermarks below would prove nothing",
            applied.events_after
        )));
    }
    let mut conn = pool.acquire().await?;
    let watermark_now = watermark(&mut conn).await?;
    if watermark_now != applied.watermark_after {
        return Err(crate::KernelError::Schema(format!(
            "the log's highest sequence was {} inside the migration and is {} after it: \
             something else is writing, and the writer lock did not hold",
            render_watermark(applied.watermark_after),
            render_watermark(watermark_now)
        )));
    }
    if applied.watermark_before != applied.watermark_after {
        return Err(crate::KernelError::Schema(format!(
            "the migration transaction saw the log's highest sequence move from {} to {}: the \
             count held, so this is not an append — an event was removed and replaced, or the \
             log was renumbered, and either way the events the log carried are not the events \
             it carries",
            render_watermark(applied.watermark_before),
            render_watermark(applied.watermark_after)
        )));
    }
    Ok(())
}

/// A watermark for an operator to read, with a word for the empty log rather
/// than a bare `None` in the middle of a sentence.
fn render_watermark(watermark: Option<Seq>) -> String {
    watermark.map_or_else(|| "absent (an empty log)".to_owned(), |seq| seq.to_string())
}

/// The privilege class a relation belongs to.
///
/// One table, both schemas. Task 4 added a `gwk_internal` fold and
/// `admin_init.rs` already had a `gwk` one, and two folds is two places for a
/// relation to be classified — or, worse, one place for it to be classified and
/// another where nobody noticed it was missing. A relation absent from this
/// match is a compile-clean, test-red failure with its own name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GrantClass {
    /// Appendable, never rewritable, never removable. The log and everything
    /// whose value is that it cannot be edited afterwards.
    History,
    /// The FSM seed the contract ships. Read, and nothing else.
    Seed,
    /// A projection the kernel rebuilds: inserted and updated, never deleted.
    Projection,
    /// The live-tree cache, the one CONTRACT table where DELETE is real — a
    /// close removes the row because the entity has no closed state, and the
    /// event log is the history a replay reproduces it from.
    TreeCache,
    /// The kernel's record of itself: which contract it carries and how it got
    /// here. Readable, and a process that could rewrite its own provenance has
    /// none.
    Provenance,
    /// The writer lease: one seeded row, moved by CAS.
    Lease,
    /// Durable delivery state: appended, updated as it settles, never removed.
    Delivery,
    /// The blob spine, the one place DELETE is granted — sweep reclaims,
    /// pins release, uploads expire, and the events referencing a blob outlive
    /// its bytes.
    Blob,
    /// Snapshots: appended and read, superseded rather than edited.
    Snapshot,
    /// Nothing at all. A sequence behind a table the runtime role cannot
    /// insert into has no counter it could need.
    Nothing,
}

impl GrantClass {
    /// (select, insert, update, delete). TRUNCATE is granted nowhere and is
    /// asserted separately, for every relation, so no class can opt into it.
    const fn privileges(self) -> (bool, bool, bool, bool) {
        match self {
            Self::History => (true, true, false, false),
            Self::Seed => (true, false, false, false),
            Self::Projection => (true, true, true, false),
            Self::TreeCache => (true, true, true, true),
            Self::Provenance => (true, false, false, false),
            Self::Lease => (true, false, true, false),
            Self::Delivery => (true, true, true, false),
            Self::Blob => (true, true, true, true),
            Self::Snapshot => (true, true, false, false),
            Self::Nothing => (false, false, false, false),
        }
    }
}

/// The class of one relation, or `None` if nothing has declared it.
fn grant_class(schema: &str, relation: &str) -> Option<GrantClass> {
    use GrantClass::{
        Blob, Delivery, History, Lease, Nothing, Projection, Provenance, Seed, Snapshot, TreeCache,
    };
    Some(match (schema, relation) {
        ("gwk", "event" | "receipt" | "ingested_record" | "cost_entry") => History,
        (
            "gwk",
            "context_manifest" | "context_release" | "context_observation" | "context_finalization",
        ) => History,
        ("gwk", "transition") => Seed,
        ("gwk", "workspace_node") => TreeCache,
        (
            "gwk",
            "attempt"
            | "attention_item"
            | "authority_grant"
            | "command"
            | "dispatch_node"
            | "engine_session"
            | "evidence"
            | "gate"
            | "lease"
            | "message"
            | "orchestrator_checkpoint"
            | "pty_session"
            | "pty_session_template"
            | "task"
            | "workflow_run"
            | "worktree",
        ) => Projection,
        ("gwk_internal", "schema_fingerprint" | "schema_migration") => Provenance,
        ("gwk_internal", "writer") => Lease,
        ("gwk_internal", "pty_delivery") => Delivery,
        ("gwk_internal", "blob" | "blob_pin" | "blob_upload") => Blob,
        ("gwk_internal", "checkpoint") => Snapshot,
        ("gwk_internal", "schema_migration_seq_seq") => Nothing,
        _ => return None,
    })
}

/// How many relations the two schemas carry when everything has been applied.
///
/// Asserted before the fold, and that ordering is the point rather than a
/// style: a per-relation sweep over a set that lost a relation agrees with
/// itself perfectly, and `all()` over an empty one is true. `admin_init.rs`
/// went red on `26 != 22` once, and the count is what fired.
const EXPECTED_RELATIONS: usize = 35;

/// R3 — every relation in both schemas holds exactly its declared privileges.
///
/// Takes the role by name rather than assuming the caller has assumed it:
/// `has_table_privilege(role, ..)` answers for a role the connection is not,
/// which is what lets the migrate verb run this over its own admin connection.
/// Generic over the executor so the applier can ask it INSIDE its own
/// transaction, where the relation set is the one the step just produced, while
/// a caller holding only a pool still asks it standalone. `&PgPool` and
/// `&mut *tx` both satisfy the bound.
pub async fn assert_grant_matrix<'e, E>(executor: E, role: &str) -> crate::Result<()>
where
    E: sqlx::PgExecutor<'e>,
{
    let rows = sqlx::query(
        "SELECT n.nspname AS schema, c.relname AS relation, \
                has_table_privilege($1, c.oid, 'SELECT')   AS sel, \
                has_table_privilege($1, c.oid, 'INSERT')   AS ins, \
                has_table_privilege($1, c.oid, 'UPDATE')   AS upd, \
                has_table_privilege($1, c.oid, 'DELETE')   AS del, \
                has_table_privilege($1, c.oid, 'TRUNCATE') AS trunc \
           FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
          WHERE n.nspname IN ('gwk', 'gwk_internal') \
            AND c.relkind IN ('r', 'p', 'S', 'v', 'm') \
          ORDER BY 1, 2",
    )
    .bind(role)
    .fetch_all(executor)
    .await?;

    if rows.len() != EXPECTED_RELATIONS {
        let found: Vec<String> = rows
            .iter()
            .map(|row| {
                format!(
                    "{}.{}",
                    row.get::<String, _>("schema"),
                    row.get::<String, _>("relation")
                )
            })
            .collect();
        return Err(crate::KernelError::Schema(format!(
            "expected {EXPECTED_RELATIONS} relations across gwk and gwk_internal and found {}: \
             {found:?}",
            rows.len()
        )));
    }

    for row in &rows {
        let schema: String = row.get("schema");
        let relation: String = row.get("relation");
        let observed = (
            row.get::<bool, _>("sel"),
            row.get::<bool, _>("ins"),
            row.get::<bool, _>("upd"),
            row.get::<bool, _>("del"),
        );
        if row.get::<bool, _>("trunc") {
            return Err(crate::KernelError::Privilege(format!(
                "{schema}.{relation}: TRUNCATE is granted nowhere"
            )));
        }
        let class = grant_class(&schema, &relation).ok_or_else(|| {
            crate::KernelError::Privilege(format!(
                "{schema}.{relation}: no declared grant class. Declare it in `grant_class` in \
                 the same commit that adds the relation — that match is the only thing between \
                 a new table and whatever privileges it happens to inherit"
            ))
        })?;
        if observed != class.privileges() {
            return Err(crate::KernelError::Privilege(format!(
                "{schema}.{relation} is {class:?} and should hold \
                 (select, insert, update, delete) = {:?}, and holds {observed:?}",
                class.privileges()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Four digests that are distinguishable at a glance and still the exact
    // shape a real one has — the resolver rejects anything else, so a
    // convenient short stand-in would only test the rejection path.
    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const D: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
    const Z: &str = "zzzzzzzz00000000000000000000000000000000000000000000000000000000";

    /// A step from `base` to `result`, named the way the generator names one.
    const fn step(id: &'static str, base: &'static str, result: &'static str) -> Step {
        Step {
            id,
            base,
            result,
            // The resolver never reads these; the applier does. A fixture that
            // carried migrations would be asserting the applier's behaviour
            // from the resolver's tests.
            backend_migrations: &[],
            sql: "SELECT 1;\n",
        }
    }

    const A_TO_B: Step = step("aaaaaaaa-bbbbbbbb.sql", A, B);
    const B_TO_C: Step = step("bbbbbbbb-cccccccc.sql", B, C);
    const C_TO_D: Step = step("cccccccc-dddddddd.sql", C, D);

    #[test]
    fn a_digest_nothing_bases_on_names_every_known_base() {
        let steps = [A_TO_B, B_TO_C, C_TO_D];
        let refusal = resolve(&steps, Z, D).expect_err("Z is not in the chain");

        let ChainRefusal::NoChain {
            from,
            to,
            reached,
            bases,
        } = &refusal
        else {
            panic!("expected NoChain, got {refusal:?}");
        };
        assert_eq!(
            from, Z,
            "the refusal carries the digest the database records"
        );
        assert_eq!(to, D, "and the one this binary carries");
        assert_eq!(reached, Z, "the walk never left the starting digest");

        // COUNT, not presence. A diagnostic that offers one candidate reads
        // exactly like a diagnostic that found one candidate, and the reader
        // who trusts it goes looking for a step that does not exist.
        assert_eq!(bases.len(), 3, "every base in the registry: {bases:?}");
        let rendered = refusal.to_string();
        assert_eq!(
            [A, B, C].iter().filter(|b| rendered.contains(*b)).count(),
            3,
            "all three bases reach the rendered message: {rendered}"
        );
    }

    #[test]
    fn two_steps_on_one_base_are_refused_before_any_walk() {
        // The requested route — C to D — never touches the fork at A. A
        // registry that branches is refused anyway, because the refusal is
        // about what this build ships, not about where this caller was going.
        let steps = [A_TO_B, step("aaaaaaaa-cccccccc.sql", A, C), C_TO_D];
        let refusal = resolve(&steps, C, D).expect_err("the registry forks at A");

        let ChainRefusal::Branching { base, ids } = &refusal else {
            panic!("expected Branching, got {refusal:?}");
        };
        assert_eq!(base, A);
        assert_eq!(ids.len(), 2, "both claimants are named: {ids:?}");
        assert_eq!(ids[0], A_TO_B.id);
        assert_eq!(ids[1], "aaaaaaaa-cccccccc.sql");
    }

    #[test]
    fn a_cycle_names_the_repeated_digest() {
        let steps = [A_TO_B, step("bbbbbbbb-aaaaaaaa.sql", B, A)];
        let refusal = resolve(&steps, A, D).expect_err("A and B loop");
        assert_eq!(
            refusal,
            ChainRefusal::Cycle {
                digest: A.to_owned()
            },
            "the refusal names the digest reached twice"
        );
    }

    #[test]
    fn a_chain_that_stops_short_of_the_target_is_refused() {
        // The registry is a perfectly good chain — it just does not end where
        // this binary is. That is the case where a resolver is most tempted to
        // apply what it has and hope.
        let steps = [A_TO_B, B_TO_C];
        let refusal = resolve(&steps, A, D).expect_err("the chain ends at C, not D");

        let ChainRefusal::NoChain { reached, from, .. } = &refusal else {
            panic!("expected NoChain, got {refusal:?}");
        };
        assert_eq!(reached, C, "the refusal points at where the chain ended");
        assert_ne!(reached, from, "not at where it began");
    }

    #[test]
    fn an_empty_registry_is_its_own_refusal() {
        let refusal = resolve(&[], A, B).expect_err("nothing is registered");
        assert_eq!(
            refusal,
            ChainRefusal::NoSteps {
                from: A.to_owned(),
                to: B.to_owned()
            }
        );

        let rendered = refusal.to_string();
        assert!(rendered.contains("no steps are registered"), "{rendered}");
        // Zero steps walked and zero steps found are the same observation, and
        // only one of them is the truth here.
        assert!(!rendered.contains("no step bases on"), "{rendered}");
    }

    #[test]
    fn the_happy_path_walks_each_step_once_in_order() {
        let steps = [A_TO_B, B_TO_C, C_TO_D];
        let chain = resolve(&steps, A, D).expect("A to D is a chain");

        assert_eq!(chain.len(), 3, "one step per hop, no more");
        let ids: Vec<&str> = chain.iter().map(|step| step.id).collect();
        assert_eq!(ids, [A_TO_B.id, B_TO_C.id, C_TO_D.id]);
    }

    #[test]
    fn the_chain_comes_out_of_the_digests_not_out_of_the_slice_order() {
        // Same three steps, shuffled. The generator emits file-name order,
        // which is not application order and is not meant to be.
        let steps = [C_TO_D, A_TO_B, B_TO_C];
        let chain = resolve(&steps, A, D).expect("A to D is still a chain");

        assert_eq!(chain.len(), 3);
        let ids: Vec<&str> = chain.iter().map(|step| step.id).collect();
        assert_eq!(ids, [A_TO_B.id, B_TO_C.id, C_TO_D.id]);
    }

    #[test]
    fn a_step_that_arrives_where_it_started_is_refused() {
        let steps = [A_TO_B, step("bbbbbbbb-bbbbbbbb.sql", B, B)];
        let refusal = resolve(&steps, A, D).expect_err("B to B is not a step");
        assert_eq!(
            refusal,
            ChainRefusal::SelfStep {
                id: "bbbbbbbb-bbbbbbbb.sql".to_owned(),
                digest: B.to_owned()
            }
        );
    }

    #[test]
    fn a_digest_that_is_not_lowercase_hex_is_refused() {
        let steps = [step("zzzzzzzz-bbbbbbbb.sql", Z, B)];
        let refusal = resolve(&steps, Z, B).expect_err("Z is not hex");
        assert_eq!(
            refusal,
            ChainRefusal::Malformed {
                id: "zzzzzzzz-bbbbbbbb.sql".to_owned(),
                field: "base",
                digest: Z.to_owned()
            }
        );
    }
}
