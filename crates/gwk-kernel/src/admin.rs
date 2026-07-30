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
/// `event`, `receipt`, and `ingested_record` are append-only and lose UPDATE —
/// the last of those is a projection the kernel DOES rebuild, but only ever by
/// inserting, so granting it UPDATE would widen the role for a write no code
/// path makes. `transition` is the FSM seed the contract ships — the kernel
/// only ever reads it, so it loses every write.
///
/// The blob tables are the one place DELETE is granted, and only inside
/// `gwk_internal`: sweep reclaims unreferenced blobs, evidence pins are
/// released, and uploads expire. None of that is history — the events that
/// REFERENCE a blob stay in the log after its bytes are gone, which is what
/// makes a swept or shredded blob auditable at all.
pub fn backend_script(role: &str, contract_sha256: &str) -> String {
    let migrations = BACKEND_MIGRATIONS.join("\n");
    format!(
        "{migrations}\n\
         INSERT INTO gwk_internal.schema_fingerprint (id, contract_sha256) \
         VALUES (1, '{contract_sha256}');\n\
         GRANT USAGE ON SCHEMA gwk TO {role};\n\
         GRANT USAGE ON SCHEMA gwk_internal TO {role};\n\
         GRANT SELECT, INSERT, UPDATE ON ALL TABLES IN SCHEMA gwk TO {role};\n\
         REVOKE UPDATE ON gwk.event, gwk.receipt, gwk.ingested_record FROM {role};\n\
         REVOKE INSERT, UPDATE ON gwk.transition FROM {role};\n\
         GRANT SELECT ON gwk_internal.schema_fingerprint TO {role};\n\
         GRANT SELECT, UPDATE ON gwk_internal.writer TO {role};\n\
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
        assert!(script.contains("REVOKE INSERT, UPDATE ON gwk.transition"));
        assert!(!script.contains("TRUNCATE"), "{script}");

        // Deletion is granted on the blob tables and NOWHERE else. The check is
        // spelled as "every granted object is one of these three" rather than
        // "the blob grant is present", because the second passes just as
        // happily while a fourth line hands out DELETE on the log.
        let granted: Vec<&str> = script
            .lines()
            .filter(|line| line.starts_with("GRANT") && line.contains("DELETE"))
            .collect();
        assert_eq!(granted.len(), 1, "{script}");
        for object in ["gwk_internal.blob", "gwk_internal.blob_pin"] {
            assert!(granted[0].contains(object), "{}", granted[0]);
        }
        assert!(!granted[0].contains(" gwk."), "{}", granted[0]);
    }
}
