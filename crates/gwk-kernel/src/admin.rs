//! One-shot initialization of a target database.
//!
//! `gw admin init` is the ONLY thing that runs DDL, and it runs against the
//! schema-owner DSN. It applies the backend-neutral contract, the PostgreSQL
//! mechanics beside it, records which contract the database now carries, and
//! grants the already-created runtime role the narrow set of privileges the
//! daemon needs.
//!
//! It refuses anything that is not an empty database (a fresh epoch starts on
//! an empty target — no import, no backfill, no adoption), with one
//! exception: re-running against a database this same contract already
//! initialized is a no-op, so a retried operator command is safe.
//!
//! Recovery from a failed init is to drop the database and create a new one.
//! That is deliberately the whole recovery story: a half-applied target is
//! indistinguishable from a stranger's, and at cutover time an empty database
//! costs one command.

use sqlx::{PgPool, Row};

use crate::config::AdminConfig;
use crate::contract_sql::{CONTRACT_SQL, CONTRACT_SQL_SHA256};
use crate::error::{KernelError, Result};

/// The PostgreSQL mechanics applied beside the contract, in order.
const BACKEND_MIGRATIONS: &[&str] = &[
    include_str!("../migrations/0001_kernel_internal.sql"),
    include_str!("../migrations/0002_writer.sql"),
    include_str!("../migrations/0003_blob.sql"),
    include_str!("../migrations/0004_checkpoint.sql"),
    include_str!("../migrations/0005_pty_delivery.sql"),
];

// ponytail: still no migration runner, and now for a better reason than "there
// is only one file". `init` is all-or-nothing against an EMPTY database, so
// there is no version ladder to walk — these are applied in order, once, in one
// transaction, or not at all. A real migrator earns its keep the first time an
// EXISTING database has to be upgraded in place; nothing before 1.0 does.

/// What a candidate target database already contains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetState {
    /// No gwk objects and no other user objects — safe to initialize.
    Empty,
    /// Already carries a gwk contract. The digest says WHICH one.
    Initialized { contract_sha256: String },
    /// Nonempty and unrecognized. Never written to.
    Foreign { objects: Vec<String> },
}

/// What [`init`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitOutcome {
    /// The contract, the backend mechanics, and the grants were applied.
    Initialized,
    /// This exact contract was already installed; nothing changed.
    AlreadyInitialized,
}

/// Role-level attributes, which exist whether or not the schema does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleAttributes {
    pub superuser: bool,
    pub create_role: bool,
    pub create_db: bool,
    pub bypass_rls: bool,
}

/// Everything the daemon checks about the credential it was handed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimePrivileges {
    pub attributes: RoleAttributes,
    pub can_update_event: bool,
    pub can_delete_event: bool,
    pub can_update_receipt: bool,
    pub can_delete_receipt: bool,
    pub can_create_in_gwk: bool,
}

impl RoleAttributes {
    /// Attributes that outrank the kernel, named. Empty means safe.
    pub fn violations(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.superuser {
            out.push("SUPERUSER");
        }
        if self.create_role {
            out.push("CREATEROLE");
        }
        if self.create_db {
            out.push("CREATEDB");
        }
        if self.bypass_rls {
            out.push("BYPASSRLS");
        }
        out
    }
}

impl RuntimePrivileges {
    /// Every privilege the kernel refuses to hold, named. Empty means safe to
    /// serve. History is append-only in the contract's own triggers too; this
    /// is the grant-level half, so a dropped trigger is not the only thing
    /// standing between a bug and a rewritten log.
    pub fn violations(&self) -> Vec<&'static str> {
        let mut out = self.attributes.violations();
        if self.can_update_event {
            out.push("UPDATE on gwk.event");
        }
        if self.can_delete_event {
            out.push("DELETE on gwk.event");
        }
        if self.can_update_receipt {
            out.push("UPDATE on gwk.receipt");
        }
        if self.can_delete_receipt {
            out.push("DELETE on gwk.receipt");
        }
        if self.can_create_in_gwk {
            out.push("CREATE on schema gwk");
        }
        out
    }
}

/// Decide what a target database is, from facts already queried out of it.
///
/// Split from the queries so the decision table is testable without a server;
/// [`inspect`] is the thin part that gathers the facts.
pub fn classify(
    gwk_present: bool,
    contract_sha256: Option<String>,
    foreign_objects: Vec<String>,
) -> TargetState {
    if !foreign_objects.is_empty() {
        return TargetState::Foreign {
            objects: foreign_objects,
        };
    }
    match (gwk_present, contract_sha256) {
        (false, None) => TargetState::Empty,
        (true, Some(contract_sha256)) => TargetState::Initialized { contract_sha256 },
        // Half a kernel. A crashed initialization and a stranger who happens
        // to have named a schema `gwk` look identical from here, and either
        // way initialization wants an empty target.
        (true, None) => TargetState::Foreign {
            objects: vec!["schema gwk (without a gwk_internal.schema_fingerprint row)".to_owned()],
        },
        (false, Some(_)) => TargetState::Foreign {
            objects: vec!["gwk_internal.schema_fingerprint (without a gwk schema)".to_owned()],
        },
    }
}

/// Read what the target database already contains.
pub async fn inspect(pool: &PgPool) -> Result<TargetState> {
    let row = sqlx::query(
        "SELECT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = 'gwk') AS gwk_present, \
         to_regclass('gwk_internal.schema_fingerprint') IS NOT NULL AS fingerprint_table",
    )
    .fetch_one(pool)
    .await?;
    let gwk_present: bool = row.try_get("gwk_present")?;
    let fingerprint_table: bool = row.try_get("fingerprint_table")?;

    let contract_sha256: Option<String> = if fingerprint_table {
        sqlx::query_scalar(
            "SELECT contract_sha256 FROM gwk_internal.schema_fingerprint WHERE id = 1",
        )
        .fetch_optional(pool)
        .await?
    } else {
        None
    };

    // Anything outside the two kernel schemas and PostgreSQL's own. Bounded:
    // the message names a handful, it does not inventory a stranger's database.
    let foreign_objects: Vec<String> = sqlx::query_scalar(
        "SELECT n.nspname || '.' || c.relname \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind IN ('r', 'p', 'v', 'm', 'S', 'f') \
           AND n.nspname NOT IN ('information_schema', 'gwk', 'gwk_internal') \
           AND n.nspname NOT LIKE 'pg\\_%' \
         ORDER BY 1 LIMIT 20",
    )
    .fetch_all(pool)
    .await?;

    Ok(classify(gwk_present, contract_sha256, foreign_objects))
}

/// Read a named role's attributes. `None` when no such role exists.
pub async fn role_attributes<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    role: &str,
) -> Result<Option<RoleAttributes>> {
    let row = sqlx::query(
        "SELECT rolsuper, rolcreaterole, rolcreatedb, rolbypassrls \
         FROM pg_roles WHERE rolname = $1",
    )
    .bind(role)
    .fetch_optional(executor)
    .await?;
    row.map(|row| {
        Ok(RoleAttributes {
            superuser: row.try_get("rolsuper")?,
            create_role: row.try_get("rolcreaterole")?,
            create_db: row.try_get("rolcreatedb")?,
            bypass_rls: row.try_get("rolbypassrls")?,
        })
    })
    .transpose()
}

/// Read what the CURRENT connection is allowed to do. The daemon calls this at
/// startup and refuses to serve while [`RuntimePrivileges::violations`] is
/// non-empty.
pub async fn runtime_privileges<'e>(
    executor: impl sqlx::PgExecutor<'e>,
) -> Result<RuntimePrivileges> {
    let row = sqlx::query(
        "SELECT rolsuper, rolcreaterole, rolcreatedb, rolbypassrls, \
           has_table_privilege('gwk.event', 'UPDATE')   AS upd_event, \
           has_table_privilege('gwk.event', 'DELETE')   AS del_event, \
           has_table_privilege('gwk.receipt', 'UPDATE') AS upd_receipt, \
           has_table_privilege('gwk.receipt', 'DELETE') AS del_receipt, \
           has_schema_privilege('gwk', 'CREATE')        AS create_gwk \
         FROM pg_roles WHERE rolname = current_user",
    )
    .fetch_one(executor)
    .await?;
    Ok(RuntimePrivileges {
        attributes: RoleAttributes {
            superuser: row.try_get("rolsuper")?,
            create_role: row.try_get("rolcreaterole")?,
            create_db: row.try_get("rolcreatedb")?,
            bypass_rls: row.try_get("rolbypassrls")?,
        },
        can_update_event: row.try_get("upd_event")?,
        can_delete_event: row.try_get("del_event")?,
        can_update_receipt: row.try_get("upd_receipt")?,
        can_delete_receipt: row.try_get("del_receipt")?,
        can_create_in_gwk: row.try_get("create_gwk")?,
    })
}

/// A receipt window, both ends resolved by the database exactly once.
///
/// Resolution is what makes a receipt reproducible: PostgreSQL accepts
/// relative timestamp literals (`yesterday`, `now`), and `now` in particular
/// is cast per statement — a bound that floated across the figure queries
/// would hand each figure a slightly different span. The resolved texts are
/// what every query binds and what the receipt records, so a relative input
/// leaves as the concrete instant it meant at read time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptWindow {
    pub start: String,
    pub end: String,
}

/// Resolve `from ..= to` against the database's clock, `to` defaulting to
/// `now()`.
///
/// An inverted window refuses rather than resolving: a transposed flag pair
/// would otherwise produce a receipt of zeros byte-identical to a
/// legitimately quiet one. Errors here are input errors — the query does
/// nothing but cast the two values — which is what lets a caller class them
/// as usage rather than storage.
pub async fn resolve_window(pool: &PgPool, from: &str, to: Option<&str>) -> Result<ReceiptWindow> {
    let row = sqlx::query(
        "SELECT x.ws::text AS ws, x.we::text AS we, x.ws > x.we AS inverted \
         FROM (SELECT $1::timestamptz AS ws, COALESCE($2::timestamptz, now()) AS we) x",
    )
    .bind(from)
    .bind(to)
    .fetch_one(pool)
    .await?;
    let inverted: bool = row.try_get("inverted")?;
    let window = ReceiptWindow {
        start: row.try_get("ws")?,
        end: row.try_get("we")?,
    };
    if inverted {
        return Err(KernelError::Config(format!(
            "the window ends before it starts ({} > {})",
            window.start, window.end
        )));
    }
    Ok(window)
}

/// The machine half of a driving-window receipt, read from the log.
///
/// The four figures a cutover receipt defines, over `gwk.pty_session` and
/// `gwk.event`. Reading them here rather than from a psql scrollback is what
/// makes a receipt's figures reproducible: the queries are these, not
/// whatever was typed at the prompt.
///
/// Two session counts ride along because a fold cannot tell "summed to zero"
/// from "summed over nothing" — and the folds here have two different
/// denominators, so one count cannot vouch for the other's figures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DrivingFigures {
    /// Sessions OPENED inside the window — the set `generations` and
    /// `restarts` fold over. Says nothing about `peak_concurrent` or the
    /// counters, which fold over the alive set below.
    pub sessions_in_window: i64,
    /// Sessions alive at any point in the window (opened before its end,
    /// not closed before its start) — the set `peak_concurrent`,
    /// `attaches`, and `detaches` fold over. Zeros beside a zero here are
    /// an empty window, not a quiet one.
    pub sessions_alive_in_window: i64,
    /// Distinct days with any pty-session ledger activity in the window.
    pub days_driven: i64,
    /// Sweep-line maximum of concurrently open sessions, intervals clipped to
    /// the window. At one instant an open sorts before a close, so a session
    /// starting the moment another ends counts both.
    pub peak_concurrent: i64,
    /// Distinct host generations among the window's sessions.
    pub generations: i64,
    /// Generation boundaries observed in the window: one less than
    /// [`generations`](Self::generations), floored at zero in Rust rather
    /// than `- 1` in SQL so an empty window reads 0, not -1.
    pub restarts: i64,
    /// Superseded generations that left sessions running — retire never
    /// arrived before the successor. Read over the whole ledger rather than
    /// the window, because a generation's crash is visible only against the
    /// generation that superseded it.
    pub crashed_generations: i64,
    /// Attach counter, summed over sessions alive at any point in the window.
    /// Counters accumulate per session lifetime, so a session spanning a
    /// window edge contributes its full totals.
    pub attaches: i64,
    /// Detach counter, on the same terms as `attaches`.
    pub detaches: i64,
}

/// Read [`DrivingFigures`] for an already-resolved window.
///
/// Every query binds [`ReceiptWindow`]'s resolved texts, so all figures
/// share one exact span by construction.
pub async fn driving_figures(pool: &PgPool, window: &ReceiptWindow) -> Result<DrivingFigures> {
    let opened = sqlx::query(
        "SELECT count(*) AS sessions, count(DISTINCT generation) AS generations \
         FROM gwk.pty_session \
         WHERE opened_at BETWEEN $1::timestamptz AND $2::timestamptz",
    )
    .bind(&window.start)
    .bind(&window.end)
    .fetch_one(pool)
    .await?;
    let sessions_in_window: i64 = opened.try_get("sessions")?;
    let generations: i64 = opened.try_get("generations")?;

    let days_driven: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT date(occurred_at)) FROM gwk.event \
         WHERE aggregate_type = 'pty_session' \
           AND occurred_at BETWEEN $1::timestamptz AND $2::timestamptz",
    )
    .bind(&window.start)
    .bind(&window.end)
    .fetch_one(pool)
    .await?;

    let peak_concurrent: i64 = sqlx::query_scalar(
        "WITH bounds AS ( \
           SELECT greatest(opened_at, $1::timestamptz) AS t, 1 AS d \
           FROM gwk.pty_session \
           WHERE opened_at <= $2::timestamptz \
             AND (closed_at IS NULL OR closed_at >= $1::timestamptz) \
           UNION ALL \
           SELECT least(closed_at, $2::timestamptz), -1 \
           FROM gwk.pty_session \
           WHERE closed_at IS NOT NULL \
             AND closed_at >= $1::timestamptz AND opened_at <= $2::timestamptz \
         ) \
         SELECT coalesce(max(running), 0)::bigint \
         FROM (SELECT sum(d) OVER (ORDER BY t, d DESC) AS running FROM bounds) s",
    )
    .bind(&window.start)
    .bind(&window.end)
    .fetch_one(pool)
    .await?;

    // No window clause, deliberately: see the field's doc. The subquery's row
    // is the newest session overall; on an empty table it yields no row, the
    // comparison is NULL, and the count is honestly zero.
    let crashed_generations: i64 = sqlx::query_scalar(
        "SELECT count(DISTINCT generation) FROM gwk.pty_session \
         WHERE closed_at IS NULL \
           AND generation <> (SELECT generation FROM gwk.pty_session \
                              ORDER BY opened_at DESC LIMIT 1)",
    )
    .fetch_one(pool)
    .await?;

    // The alive count comes from the same statement as the sums it vouches
    // for, so the denominator and the folds cannot read different sets.
    let counters = sqlx::query(
        "SELECT count(*) AS alive, \
                coalesce(sum(attach_count), 0)::bigint AS attaches, \
                coalesce(sum(detach_count), 0)::bigint AS detaches \
         FROM gwk.pty_session \
         WHERE opened_at <= $2::timestamptz \
           AND (closed_at IS NULL OR closed_at >= $1::timestamptz)",
    )
    .bind(&window.start)
    .bind(&window.end)
    .fetch_one(pool)
    .await?;

    Ok(DrivingFigures {
        sessions_in_window,
        sessions_alive_in_window: counters.try_get("alive")?,
        days_driven,
        peak_concurrent,
        generations,
        restarts: (generations - 1).max(0),
        crashed_generations,
        attaches: counters.try_get("attaches")?,
        detaches: counters.try_get("detaches")?,
    })
}

/// The backend mechanics, the fingerprint row, and the runtime grants, as one
/// script.
///
/// `role` is interpolated rather than bound because PostgreSQL cannot
/// parameterize an identifier. [`crate::config::validate_role`] has already
/// restricted it to `[a-z_][a-z0-9_]*`, which needs no quoting and cannot
/// carry a separator, a quote, or a comment.
///
/// The privilege list grants exactly what the daemon does: read everything,
/// append history, and update the projections it rebuilds. Nothing in the
/// CONTRACT schema is deletable or truncatable, so the log never shrinks.
/// `event`, `receipt`, `ingested_record`, and `cost_entry` are append-only and
/// lose UPDATE — the last two are projections the kernel DOES rebuild, but only
/// ever by inserting, so granting them UPDATE would widen the role for a write
/// no code path makes. `transition` is the FSM seed the contract ships — the
/// kernel only ever reads it, so it loses every write.
///
/// The four Context truth records lose UPDATE for a stronger reason than the
/// others: immutability is their definition, not an optimization. A resolved
/// manifest is what an independent verifier checks and what Explain and Compare
/// reconstruct from; a manifest that can be edited after the fact verifies
/// nothing, because the row a verifier reads is no longer the row the attempt
/// ran against. The blanket `GRANT ... UPDATE ON ALL TABLES` reaches every new
/// table in the schema by construction, so a record is mutable the moment it is
/// created unless this line names it. That default is the right one for a
/// schema that is mostly rebuildable projections, and it is exactly wrong here.
///
/// The blob tables are the one place DELETE is granted, and only inside
/// `gwk_internal`: sweep reclaims unreferenced blobs, evidence pins are
/// released, and uploads expire. None of that is history — the events that
/// REFERENCE a blob stay in the log after its bytes are gone, which is what
/// makes a swept or shredded blob auditable at all.
///
/// `workspace_node` is the one CONTRACT table that joins them: a close is a
/// real DELETE because the entity has no closed state — the row is a cache of
/// the live tree, the `workspace_node_closed` event is the history, and a
/// replay reproduces the same deletion. The parentage trigger and the parent
/// FK stay ENABLE ALWAYS, so the grant widens nothing about tree legality.
pub fn backend_script(role: &str, contract_sha256: &str) -> String {
    let migrations = BACKEND_MIGRATIONS.join("\n");
    format!(
        "{migrations}\n\
         INSERT INTO gwk_internal.schema_fingerprint (id, contract_sha256) \
         VALUES (1, '{contract_sha256}');\n\
         GRANT USAGE ON SCHEMA gwk TO {role};\n\
         GRANT USAGE ON SCHEMA gwk_internal TO {role};\n\
         GRANT SELECT, INSERT, UPDATE ON ALL TABLES IN SCHEMA gwk TO {role};\n\
         REVOKE UPDATE ON gwk.event, gwk.receipt, gwk.ingested_record, \
           gwk.cost_entry FROM {role};\n\
         REVOKE UPDATE ON gwk.context_manifest, gwk.context_release, \
           gwk.context_observation, gwk.context_finalization FROM {role};\n\
         REVOKE INSERT, UPDATE ON gwk.transition FROM {role};\n\
         GRANT DELETE ON gwk.workspace_node TO {role};\n\
         GRANT SELECT ON gwk_internal.schema_fingerprint TO {role};\n\
         GRANT SELECT, UPDATE ON gwk_internal.writer TO {role};\n\
         GRANT SELECT, INSERT, UPDATE ON gwk_internal.pty_delivery TO {role};\n\
         GRANT SELECT, INSERT, UPDATE, DELETE ON \
           gwk_internal.blob, gwk_internal.blob_pin, gwk_internal.blob_upload TO {role};\n\
         GRANT SELECT, INSERT ON gwk_internal.checkpoint TO {role};\n"
    )
}

/// Initialize `admin.admin_database_url()`'s database, or explain why not.
pub async fn init(pool: &PgPool, admin: &AdminConfig) -> Result<InitOutcome> {
    let role = admin.runtime_role();
    let attributes = role_attributes(pool, role).await?.ok_or_else(|| {
        KernelError::Privilege(format!(
            "role {role:?} does not exist: initialization grants an already-created role and \
             never creates one"
        ))
    })?;
    let violations = attributes.violations();
    if !violations.is_empty() {
        return Err(KernelError::Privilege(format!(
            "role {role:?} holds {}: the kernel refuses to run as a role that can re-grant or \
             re-DDL its own store",
            violations.join(", ")
        )));
    }

    match inspect(pool).await? {
        TargetState::Initialized { contract_sha256 } if contract_sha256 == CONTRACT_SQL_SHA256 => {
            return Ok(InitOutcome::AlreadyInitialized);
        }
        TargetState::Initialized { contract_sha256 } => {
            return Err(KernelError::Schema(format!(
                "this database carries contract {contract_sha256}, and this binary carries \
                 {CONTRACT_SQL_SHA256} — initialize a fresh database with the matching build"
            )));
        }
        TargetState::Foreign { objects } => {
            return Err(KernelError::Schema(format!(
                "refusing to initialize a nonempty database this binary does not recognize; it \
                 already contains: {}. Create a fresh empty database and point \
                 GWK_ADMIN_DATABASE_URL at that",
                objects.join(", ")
            )));
        }
        TargetState::Empty => {}
    }

    // The contract script wraps itself in BEGIN/COMMIT, so it commits alone.
    // Everything after it goes in a single simple-query batch, which
    // PostgreSQL runs as one implicit transaction.
    sqlx::raw_sql(CONTRACT_SQL).execute(pool).await?;
    // The audit sqlx::AssertSqlSafe demands: the only runtime-substituted
    // values in this script are `role`, restricted to `[a-z_][a-z0-9_]*` by
    // config::validate_role, and a digest this binary computed over its own
    // embedded DDL. Neither can carry a quote, a separator, or a comment.
    sqlx::raw_sql(sqlx::AssertSqlSafe(backend_script(
        role,
        CONTRACT_SQL_SHA256,
    )))
    .execute(pool)
    .await?;
    Ok(InitOutcome::Initialized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_database_is_the_only_thing_init_will_write_to() {
        assert_eq!(classify(false, None, vec![]), TargetState::Empty);
        assert_eq!(
            classify(true, Some("abc".to_owned()), vec![]),
            TargetState::Initialized {
                contract_sha256: "abc".to_owned()
            }
        );
    }

    #[test]
    fn every_incoherent_or_occupied_target_is_refused() {
        // A stranger's table, even beside a complete kernel.
        let foreign = classify(
            true,
            Some("abc".to_owned()),
            vec!["public.users".to_owned()],
        );
        assert_eq!(
            foreign,
            TargetState::Foreign {
                objects: vec!["public.users".to_owned()]
            }
        );
        // Half-applied in either direction is a refusal, not a resume.
        for state in [
            classify(true, None, vec![]),
            classify(false, Some("abc".to_owned()), vec![]),
        ] {
            assert!(
                matches!(state, TargetState::Foreign { .. }),
                "expected a refusal, got {state:?}"
            );
        }
    }

    #[test]
    fn a_role_that_outranks_the_kernel_names_every_reason() {
        let clean = RoleAttributes {
            superuser: false,
            create_role: false,
            create_db: false,
            bypass_rls: false,
        };
        assert!(clean.violations().is_empty());
        let all = RoleAttributes {
            superuser: true,
            create_role: true,
            create_db: true,
            bypass_rls: true,
        };
        assert_eq!(
            all.violations(),
            ["SUPERUSER", "CREATEROLE", "CREATEDB", "BYPASSRLS"]
        );

        let mut privileges = RuntimePrivileges {
            attributes: clean,
            can_update_event: false,
            can_delete_event: false,
            can_update_receipt: false,
            can_delete_receipt: false,
            can_create_in_gwk: false,
        };
        assert!(privileges.violations().is_empty());
        privileges.can_delete_event = true;
        privileges.can_create_in_gwk = true;
        assert_eq!(
            privileges.violations(),
            ["DELETE on gwk.event", "CREATE on schema gwk"]
        );
    }

    #[test]
    fn the_grant_script_withholds_history_mutation_and_all_deletion() {
        let script = backend_script("gwk_runtime", &"a".repeat(64));
        assert!(script.contains("CREATE SCHEMA IF NOT EXISTS gwk_internal;"));
        assert!(script.contains("VALUES (1, '"));
        assert!(script.contains("GRANT SELECT, INSERT, UPDATE ON ALL TABLES IN SCHEMA gwk"));
        assert!(script.contains("REVOKE UPDATE ON gwk.event, gwk.receipt"));
        assert!(script.contains("REVOKE UPDATE ON gwk.context_manifest, gwk.context_release"));
        assert!(script.contains("gwk.context_observation, gwk.context_finalization FROM"));
        assert!(script.contains("REVOKE INSERT, UPDATE ON gwk.transition"));
        assert!(script.contains("GRANT SELECT, INSERT, UPDATE ON gwk_internal.pty_delivery"));
        assert!(!script.contains("TRUNCATE"), "{script}");

        // Deletion is granted on the blob tables and on gwk.workspace_node —
        // NOWHERE else. The check is spelled as "every DELETE-granting line is
        // one of the two known lines" rather than "the known grants are
        // present", because the second passes just as happily while a third
        // line hands out DELETE on the log. workspace_node earns its place:
        // close is a real DELETE of a live-tree cache row whose history is the
        // event log (see `backend_script`'s doc).
        let granted: Vec<&str> = script
            .lines()
            .filter(|line| line.starts_with("GRANT") && line.contains("DELETE"))
            .collect();
        assert_eq!(granted.len(), 2, "{script}");
        let workspace = granted
            .iter()
            .find(|line| line.contains("gwk.workspace_node"))
            .expect("the workspace_node DELETE grant");
        assert!(
            workspace.starts_with("GRANT DELETE ON gwk.workspace_node TO "),
            "{workspace}"
        );
        let blob = granted
            .iter()
            .find(|line| line.contains("gwk_internal.blob"))
            .expect("the blob DELETE grant");
        for object in ["gwk_internal.blob", "gwk_internal.blob_pin"] {
            assert!(blob.contains(object), "{blob}");
        }
        assert!(!blob.contains(" gwk."), "{blob}");
    }
}
