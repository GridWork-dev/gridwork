//! Process configuration, read from the environment.
//!
//! The two DSNs are deliberately not interchangeable. `GWK_ADMIN_DATABASE_URL`
//! owns the schema and exists only for the one-shot `admin init`;
//! `GWK_DATABASE_URL` is the least-privilege runtime credential the daemon
//! receives. A daemon that can reach the admin DSN can re-DDL its own store, so
//! [`KernelConfig::from_env`] REFUSES to start when the admin variable is
//! present rather than trusting itself to ignore it. Putting both in one unit
//! file is the mistake this catches.

use std::path::{Path, PathBuf};

use secrecy::SecretString;

use crate::error::{KernelError, Result};

/// The runtime (least-privilege) connection string.
pub const DATABASE_URL_ENV: &str = "GWK_DATABASE_URL";
/// The schema-owner connection string. One-shot initialization only.
pub const ADMIN_DATABASE_URL_ENV: &str = "GWK_ADMIN_DATABASE_URL";
/// The already-created role the runtime credential authenticates as.
pub const RUNTIME_ROLE_ENV: &str = "GWK_RUNTIME_ROLE";
/// Where the daemon binds its Unix domain socket.
pub const SOCKET_PATH_ENV: &str = "GWK_SOCKET_PATH";

/// The default socket path (ADR 0002: UDS only, no network listener).
pub const DEFAULT_SOCKET_PATH: &str = "/run/gridwork/gwk.sock";

/// Longest legal PostgreSQL identifier — `NAMEDATALEN - 1`.
pub const MAX_IDENTIFIER_BYTES: usize = 63;

/// The daemon's configuration.
///
/// `Debug` is safe to log: the DSN is a [`SecretString`], which redacts itself.
#[derive(Debug)]
pub struct KernelConfig {
    database_url: SecretString,
    socket_path: PathBuf,
}

/// The one-shot initializer's configuration.
#[derive(Debug)]
pub struct AdminConfig {
    admin_database_url: SecretString,
    runtime_role: String,
}

impl KernelConfig {
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(env_lookup)
    }

    /// The env-independent half. Tests drive the rules through this instead of
    /// `set_var`, which is unsafe in edition 2024 and races parallel tests.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        if get(ADMIN_DATABASE_URL_ENV).is_some() {
            return Err(KernelError::Config(format!(
                "{ADMIN_DATABASE_URL_ENV} is set: the schema-owner credential is for one-shot \
                 `gw admin init` only and must never reach the daemon's environment"
            )));
        }
        let database_url = database_url(&get, DATABASE_URL_ENV)?;
        let socket_path = get(SOCKET_PATH_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET_PATH));
        if socket_path.as_os_str().is_empty() {
            return Err(KernelError::Config(format!("{SOCKET_PATH_ENV} is empty")));
        }
        Ok(Self {
            database_url,
            socket_path,
        })
    }

    pub fn database_url(&self) -> &SecretString {
        &self.database_url
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl AdminConfig {
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(env_lookup)
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let admin_database_url = database_url(&get, ADMIN_DATABASE_URL_ENV)?;
        let runtime_role = get(RUNTIME_ROLE_ENV).ok_or_else(|| {
            KernelError::Config(format!(
                "{RUNTIME_ROLE_ENV} is not set: initialization grants an ALREADY-CREATED runtime \
                 role and never creates one"
            ))
        })?;
        validate_role(&runtime_role)?;
        Ok(Self {
            admin_database_url,
            runtime_role,
        })
    }

    pub fn admin_database_url(&self) -> &SecretString {
        &self.admin_database_url
    }

    /// The runtime role, already validated as a bare lowercase identifier.
    pub fn runtime_role(&self) -> &str {
        &self.runtime_role
    }
}

fn env_lookup(key: &str) -> Option<String> {
    std::env::var(key).ok()
}

/// A connection string must announce itself as PostgreSQL. Without this, a
/// swapped variable reaches the driver as an opaque parse error at connect
/// time instead of a named one at startup.
fn database_url(get: &impl Fn(&str) -> Option<String>, key: &str) -> Result<SecretString> {
    let raw = get(key).ok_or_else(|| KernelError::Config(format!("{key} is not set")))?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(KernelError::Config(format!("{key} is empty")));
    }
    if !trimmed.starts_with("postgres://") && !trimmed.starts_with("postgresql://") {
        return Err(KernelError::Config(format!(
            "{key} is not a PostgreSQL URL (expected a postgres:// or postgresql:// scheme)"
        )));
    }
    Ok(SecretString::from(trimmed.to_owned()))
}

/// PostgreSQL cannot bind an identifier as a parameter, so the role name is
/// interpolated into the GRANT script. This allowlist is what keeps that safe:
/// a bare lowercase identifier needs no quoting and cannot carry a statement
/// separator, a quote, or a comment.
pub fn validate_role(role: &str) -> Result<()> {
    let invalid = |why: &str| {
        Err(KernelError::Config(format!(
            "{RUNTIME_ROLE_ENV} {why}: expected a bare lowercase identifier matching \
             [a-z_][a-z0-9_]* of at most {MAX_IDENTIFIER_BYTES} bytes, got {role:?}"
        )))
    };
    if role.is_empty() {
        return invalid("is empty");
    }
    if role.len() > MAX_IDENTIFIER_BYTES {
        return invalid("is too long");
    }
    let mut bytes = role.bytes();
    let first = bytes.next().unwrap_or(b'0');
    if !(first.is_ascii_lowercase() || first == b'_') {
        return invalid("does not start with a lowercase letter or underscore");
    }
    if !bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
        return invalid("contains a character outside [a-z0-9_]");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use secrecy::ExposeSecret;

    use super::*;

    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> + use<> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |key| {
            owned
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.to_owned())
        }
    }

    const DSN: &str = "postgres://gwk@/gridwork";
    const ADMIN_DSN: &str = "postgres://owner@/gridwork";

    #[test]
    fn the_daemon_refuses_to_start_beside_the_admin_credential() {
        let err = KernelConfig::from_lookup(lookup(&[
            (DATABASE_URL_ENV, DSN),
            (ADMIN_DATABASE_URL_ENV, ADMIN_DSN),
        ]))
        .expect_err("both DSNs present must refuse");
        assert!(
            err.to_string().contains(ADMIN_DATABASE_URL_ENV),
            "the error must name the offending variable: {err}"
        );
    }

    #[test]
    fn the_daemon_defaults_its_socket_and_keeps_its_dsn() {
        let cfg = KernelConfig::from_lookup(lookup(&[(DATABASE_URL_ENV, DSN)])).expect("config");
        assert_eq!(cfg.socket_path(), Path::new(DEFAULT_SOCKET_PATH));
        assert_eq!(cfg.database_url().expose_secret(), DSN);

        let cfg = KernelConfig::from_lookup(lookup(&[
            (DATABASE_URL_ENV, DSN),
            (SOCKET_PATH_ENV, "/tmp/gwk.sock"),
        ]))
        .expect("config");
        assert_eq!(cfg.socket_path(), Path::new("/tmp/gwk.sock"));
    }

    #[test]
    fn a_missing_or_non_postgres_dsn_is_named_at_startup() {
        let err = KernelConfig::from_lookup(lookup(&[])).expect_err("missing DSN");
        assert!(err.to_string().contains(DATABASE_URL_ENV), "{err}");

        for bad in ["", "   ", "mysql://x/y", "/var/run/postgres", "gridwork"] {
            let err = KernelConfig::from_lookup(lookup(&[(DATABASE_URL_ENV, bad)]))
                .expect_err("non-postgres DSN must refuse");
            assert!(err.to_string().contains(DATABASE_URL_ENV), "{bad:?}: {err}");
        }
        // Both spellings the driver accepts.
        for good in ["postgres://gwk@/db", "postgresql://gwk@/db"] {
            KernelConfig::from_lookup(lookup(&[(DATABASE_URL_ENV, good)])).expect(good);
        }
    }

    #[test]
    fn the_admin_needs_a_role_to_grant() {
        let err = AdminConfig::from_lookup(lookup(&[(ADMIN_DATABASE_URL_ENV, ADMIN_DSN)]))
            .expect_err("missing role");
        assert!(err.to_string().contains(RUNTIME_ROLE_ENV), "{err}");

        let cfg = AdminConfig::from_lookup(lookup(&[
            (ADMIN_DATABASE_URL_ENV, ADMIN_DSN),
            (RUNTIME_ROLE_ENV, "gwk_runtime"),
        ]))
        .expect("config");
        assert_eq!(cfg.runtime_role(), "gwk_runtime");
        assert_eq!(cfg.admin_database_url().expose_secret(), ADMIN_DSN);
    }

    #[test]
    fn only_a_bare_lowercase_identifier_reaches_the_grant_script() {
        for good in ["gwk_runtime", "_x", "r0", "a".repeat(63).as_str()] {
            validate_role(good).unwrap_or_else(|e| panic!("{good:?} should be legal: {e}"));
        }
        // Every rejection below would otherwise be interpolated into a GRANT.
        for bad in [
            "",
            "0leading",
            "Upper",
            "with-dash",
            "with space",
            "quote\"d",
            "semi;colon",
            "dash--comment",
            "role; DROP SCHEMA gwk CASCADE",
            "a".repeat(64).as_str(),
        ] {
            validate_role(bad).expect_err(&format!("{bad:?} must be refused"));
        }
    }
}
