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

use base64::prelude::{BASE64_STANDARD, Engine as _};
use gwk_domain::context::{ContentClass, RetentionClass};
use secrecy::{SecretBox, SecretString};

use crate::blob::container::DEK_BYTES;
use crate::error::{KernelError, Result};

/// The runtime (least-privilege) connection string.
pub const DATABASE_URL_ENV: &str = "GWK_DATABASE_URL";
/// The schema-owner connection string. One-shot initialization only.
pub const ADMIN_DATABASE_URL_ENV: &str = "GWK_ADMIN_DATABASE_URL";
/// The already-created role the runtime credential authenticates as.
pub const RUNTIME_ROLE_ENV: &str = "GWK_RUNTIME_ROLE";
/// Where the daemon binds its Unix domain socket.
pub const SOCKET_PATH_ENV: &str = "GWK_SOCKET_PATH";

/// Where blob containers are written. Required, absolute, no default.
pub const BLOB_ROOT_ENV: &str = "GWK_BLOB_ROOT";
/// The key-encryption key, base64 over exactly [`DEK_BYTES`] bytes.
pub const BLOB_KEK_ENV: &str = "GWK_BLOB_KEK";
/// The nonsecret label recorded beside every blob this KEK wraps.
pub const BLOB_KEK_ID_ENV: &str = "GWK_BLOB_KEK_ID";
/// Full-fidelity age retained for unpinned `pty_recording` evidence blobs.
pub const PTY_RECORDING_RETENTION_DAYS_ENV: &str = "GWK_PTY_RECORDING_RETENTION_DAYS";
/// The key a rotation is moving TO, same encoding as [`BLOB_KEK_ENV`].
///
/// Its own variable, read only by `gw admin blob rotate`, because a rotation is
/// the one operation that needs both keys at once while every other process
/// needs exactly one. Carrying the incoming key in the running key's variable
/// would leave nothing able to say which of the two it was holding.
pub const BLOB_KEK_NEXT_ENV: &str = "GWK_BLOB_KEK_NEXT";

/// The recorded policy default. Evidence pins override this age indefinitely.
pub const DEFAULT_PTY_RECORDING_RETENTION_DAYS: i32 = 30;

/// The variables carrying one Context content class's KEK and its nonsecret
/// label (R19: one key-encryption key per content class).
///
/// An explicit arm per class and no wildcard, so a new content class fails
/// this match at compile time and names its variables here before anything
/// can construct a store that silently lacks its key.
pub const fn context_kek_env(class: ContentClass) -> (&'static str, &'static str) {
    match class {
        ContentClass::Conformance => (
            "GWK_CONTEXT_KEK_CONFORMANCE",
            "GWK_CONTEXT_KEK_CONFORMANCE_ID",
        ),
        ContentClass::Private => ("GWK_CONTEXT_KEK_PRIVATE", "GWK_CONTEXT_KEK_PRIVATE_ID"),
    }
}

/// The variable carrying one retention class's window in days, or `None` for
/// a class that age can never reclaim.
///
/// A bounded class whose variable is absent has NO window — the sweep keeps
/// its blobs. Retention is an opt-in policy per deployment; the class SET is
/// contract, the numbers are not, and an unconfigured deployment fails safe
/// toward keeping bytes.
pub const fn context_retention_env(class: RetentionClass) -> Option<&'static str> {
    match class {
        RetentionClass::Permanent => None,
        RetentionClass::Manifest => Some("GWK_CONTEXT_RETENTION_DAYS_MANIFEST"),
        RetentionClass::Release => Some("GWK_CONTEXT_RETENTION_DAYS_RELEASE"),
        RetentionClass::Observation => Some("GWK_CONTEXT_RETENTION_DAYS_OBSERVATION"),
    }
}

/// The default socket path (ADR 0002: UDS only, no network listener).
pub const DEFAULT_SOCKET_PATH: &str = "/run/gridwork/gwk.sock";

/// Longest legal PostgreSQL identifier — `NAMEDATALEN - 1`.
pub const MAX_IDENTIFIER_BYTES: usize = 63;

/// Longest legal KEK label. It is copied into every container header, so it is
/// kept short on purpose — this is a name, not a place to stash material.
pub const MAX_KEK_ID_BYTES: usize = 64;

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

/// The blob spine's configuration.
///
/// All three variables are required and none has a default. A default root
/// would put ciphertext somewhere nobody chose, and a default KEK would be a
/// key everyone shares — the failure mode being avoided is a deployment that
/// starts successfully while storing blobs it cannot protect.
///
/// `Debug` is safe to log: the KEK is a [`SecretBox`], which redacts itself.
/// The label beside it is deliberately NOT secret, because it has to travel in
/// the clear inside every container header.
#[derive(Debug)]
pub struct BlobConfig {
    root: PathBuf,
    kek: SecretBox<[u8; DEK_BYTES]>,
    kek_id: String,
    pty_recording_retention_days: i32,
    /// Configured Context retention windows, one entry per BOUNDED class the
    /// deployment opted into. Read by the sweep; a class with no entry is
    /// retained. Lives here rather than on [`ContextBlobConfig`] because the
    /// sweep is the MAIN store's operation — retention policy belongs with the
    /// process that enforces it.
    context_retention: Vec<(RetentionClass, i32)>,
}

impl BlobConfig {
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(env_lookup)
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let raw_root = get(BLOB_ROOT_ENV)
            .ok_or_else(|| KernelError::Config(format!("{BLOB_ROOT_ENV} is not set")))?;
        let root = PathBuf::from(raw_root.trim());
        if root.as_os_str().is_empty() {
            return Err(KernelError::Config(format!("{BLOB_ROOT_ENV} is empty")));
        }
        // Absolute only. A relative root resolves against whatever directory
        // the process happened to start in, so the same unit file would store
        // blobs in two places and find them in neither.
        if !root.is_absolute() {
            return Err(KernelError::Config(format!(
                "{BLOB_ROOT_ENV} must be an absolute path, got {root:?}"
            )));
        }

        let kek = read_kek(BLOB_KEK_ENV, &get)?;

        let kek_id = get(BLOB_KEK_ID_ENV)
            .ok_or_else(|| KernelError::Config(format!("{BLOB_KEK_ID_ENV} is not set")))?;
        validate_kek_id(&kek_id)?;

        let pty_recording_retention_days = match get(PTY_RECORDING_RETENTION_DAYS_ENV) {
            None => DEFAULT_PTY_RECORDING_RETENTION_DAYS,
            Some(value) => positive_days(PTY_RECORDING_RETENTION_DAYS_ENV, &value)?,
        };

        // Only classes whose variable is set get a window; the rest are
        // retained. No default number: the pty default above is a recorded
        // legacy policy, and repeating that shape here would have this file
        // choosing how long a deployment keeps content nobody configured.
        let mut context_retention = Vec::new();
        for class in RetentionClass::ALL {
            let Some(name) = context_retention_env(class) else {
                continue;
            };
            if let Some(value) = get(name) {
                context_retention.push((class, positive_days(name, &value)?));
            }
        }

        Ok(Self {
            root,
            kek: SecretBox::new(kek),
            kek_id,
            pty_recording_retention_days,
            context_retention,
        })
    }

    /// The key a rotation is moving to.
    ///
    /// Read separately rather than as a fourth field, because it is required by
    /// exactly one verb and absent everywhere else — a `BlobConfig` that carried
    /// an `Option` for it would make every other caller of this type look like
    /// it might rotate.
    pub fn next_kek_from_env() -> Result<SecretBox<[u8; DEK_BYTES]>> {
        Self::next_kek_from_lookup(env_lookup)
    }

    pub fn next_kek_from_lookup(
        get: impl Fn(&str) -> Option<String>,
    ) -> Result<SecretBox<[u8; DEK_BYTES]>> {
        Ok(SecretBox::new(read_kek(BLOB_KEK_NEXT_ENV, &get)?))
    }

    /// Build a config directly, for tests and for a caller that already holds
    /// the key material.
    pub fn new(root: PathBuf, kek: [u8; DEK_BYTES], kek_id: String) -> Result<Self> {
        validate_kek_id(&kek_id)?;
        Ok(Self {
            root,
            kek: SecretBox::new(Box::new(kek)),
            kek_id,
            pty_recording_retention_days: DEFAULT_PTY_RECORDING_RETENTION_DAYS,
            context_retention: Vec::new(),
        })
    }

    /// The same config with these Context retention windows — the test-side
    /// twin of the `GWK_CONTEXT_RETENTION_DAYS_*` variables.
    pub fn with_context_retention(mut self, windows: Vec<(RetentionClass, i32)>) -> Self {
        self.context_retention = windows;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn kek(&self) -> &SecretBox<[u8; DEK_BYTES]> {
        &self.kek
    }

    pub fn kek_id(&self) -> &str {
        &self.kek_id
    }

    pub fn pty_recording_retention_days(&self) -> i32 {
        self.pty_recording_retention_days
    }

    /// The configured Context retention windows. A class absent here has no
    /// window and its blobs are retained.
    pub fn context_retention(&self) -> &[(RetentionClass, i32)] {
        &self.context_retention
    }
}

/// The per-class Context KEK material (R19), read from the environment.
///
/// R18's custody answer, formalized: every key is supplied through the
/// process environment — in deployment, a root-owned environment file the
/// service manager loads — and only the nonsecret labels are ever persisted
/// (each travels in the clear inside the container headers of the blobs its
/// key wraps). No key touches the database, which is D4's one MUST; key and
/// ciphertext sharing a host remains the disclosed residual the 8B
/// certification review re-asks with evidence.
///
/// Construction is all-or-nothing over [`ContentClass::ALL`]: a deployment
/// missing any class's key or label is refused AT PROCESS START with the
/// variable named, never at the first write that happens to need it. That is
/// the fail-closed half of "one KEK per content class" — a store that could
/// come up with half its key ring would classify content it cannot protect.
///
/// `Debug` is safe to log: every key is a [`SecretBox`], which redacts itself.
#[derive(Debug)]
pub struct ContextBlobConfig {
    keks: Vec<(ContentClass, SecretBox<[u8; DEK_BYTES]>, String)>,
}

impl ContextBlobConfig {
    pub fn from_env() -> Result<Self> {
        Self::from_lookup(env_lookup)
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self> {
        let mut keks: Vec<(ContentClass, SecretBox<[u8; DEK_BYTES]>, String)> = Vec::new();
        for class in ContentClass::ALL {
            let (kek_var, id_var) = context_kek_env(class);
            let kek = read_kek(kek_var, &get)?;
            let kek_id =
                get(id_var).ok_or_else(|| KernelError::Config(format!("{id_var} is not set")))?;
            validate_kek_id_for(id_var, &kek_id)?;
            // One label names one key. Rotation matches on the label and the
            // container header carries it, so two classes sharing a label
            // would make "which key wraps this blob" unanswerable from the
            // header — the exact question the label exists to answer.
            if let Some((other, _, _)) = keks.iter().find(|(_, _, seen)| *seen == kek_id) {
                return Err(KernelError::Config(format!(
                    "{id_var} carries {kek_id:?}, already used by the {} class: every content \
                     class's KEK label must be distinct",
                    other.as_str()
                )));
            }
            keks.push((class, SecretBox::new(kek), kek_id));
        }
        Ok(Self { keks })
    }

    /// Build directly from held material, for tests. The same completeness
    /// rule as `from_lookup`: every content class, exactly once, distinct
    /// labels.
    pub fn new(material: Vec<(ContentClass, [u8; DEK_BYTES], String)>) -> Result<Self> {
        let mut keks: Vec<(ContentClass, SecretBox<[u8; DEK_BYTES]>, String)> = Vec::new();
        for (class, kek, kek_id) in material {
            validate_kek_id_for("context kek id", &kek_id)?;
            if keks.iter().any(|(seen, _, _)| *seen == class)
                || keks.iter().any(|(_, _, seen)| *seen == kek_id)
            {
                return Err(KernelError::Config(format!(
                    "duplicate context class or kek label ({}, {kek_id:?})",
                    class.as_str()
                )));
            }
            keks.push((class, SecretBox::new(Box::new(kek)), kek_id));
        }
        if keks.len() != ContentClass::ALL.len() {
            return Err(KernelError::Config(format!(
                "context kek material covers {} of {} content classes",
                keks.len(),
                ContentClass::ALL.len()
            )));
        }
        Ok(Self { keks })
    }

    /// The class's key and its nonsecret label.
    pub fn kek(&self, class: ContentClass) -> (&SecretBox<[u8; DEK_BYTES]>, &str) {
        match self.keks.iter().find(|(seen, _, _)| *seen == class) {
            Some((_, kek, kek_id)) => (kek, kek_id),
            // Construction covers ContentClass::ALL, refused otherwise.
            None => unreachable!("ContextBlobConfig is constructed over every content class"),
        }
    }
}

/// Parse a positive whole number of days, named by the variable it came from.
fn positive_days(name: &str, value: &str) -> Result<i32> {
    value
        .trim()
        .parse::<i32>()
        .ok()
        .filter(|days| *days > 0)
        .ok_or_else(|| {
            KernelError::Config(format!("{name} must be a positive whole number of days"))
        })
}

/// Decode one base64 KEK, named by the variable it came from.
///
/// Boxed rather than returned by value so the bytes are written once, into the
/// allocation the [`SecretBox`] will own, instead of being copied off the stack
/// on the way there.
fn read_kek(name: &str, get: &impl Fn(&str) -> Option<String>) -> Result<Box<[u8; DEK_BYTES]>> {
    let encoded = get(name).ok_or_else(|| KernelError::Config(format!("{name} is not set")))?;
    let decoded = BASE64_STANDARD
        .decode(encoded.trim())
        // The error is not included: it reports positions and lengths of the
        // value being parsed, and that value is a key.
        .map_err(|_| KernelError::Config(format!("{name} is not valid base64")))?;
    if decoded.len() != DEK_BYTES {
        return Err(KernelError::Config(format!(
            "{name} decodes to {} bytes, expected exactly {DEK_BYTES}",
            decoded.len()
        )));
    }
    let mut kek = Box::new([0u8; DEK_BYTES]);
    kek.copy_from_slice(&decoded);
    Ok(kek)
}

/// The label is written into every container header and is what a rotation
/// matches on, so it stays a plain short name: no separators to confuse a
/// parser, nothing that could carry material, nothing that changes meaning
/// under a different locale.
pub fn validate_kek_id(kek_id: &str) -> Result<()> {
    validate_kek_id_for(BLOB_KEK_ID_ENV, kek_id)
}

/// The same rule, naming the variable that carried the label — the Context
/// classes each have their own, and an error blaming [`BLOB_KEK_ID_ENV`] for
/// a value it never held would send the operator to the wrong line.
pub fn validate_kek_id_for(name: &str, kek_id: &str) -> Result<()> {
    let invalid = |why: &str| {
        Err(KernelError::Config(format!(
            "{name} {why}: expected 1..={MAX_KEK_ID_BYTES} bytes matching \
             [A-Za-z0-9._-], got {kek_id:?}"
        )))
    };
    if kek_id.is_empty() {
        return invalid("is empty");
    }
    if kek_id.len() > MAX_KEK_ID_BYTES {
        return invalid("is too long");
    }
    if !kek_id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
    {
        return invalid("contains a character outside [A-Za-z0-9._-]");
    }
    Ok(())
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
    // Four strings that match the pattern above and are not identifiers.
    // PostgreSQL reads them as `RoleSpec` keywords wherever a role name is
    // expected, so `GRANT ... TO public` does not grant to a role called
    // "public" — it grants to PUBLIC, which is every role in the cluster, and
    // the privilege matrix this validator guards would land on all of them.
    // The other three resolve to whoever happens to be connected, which makes
    // the grant depend on the credential that ran the verb rather than on
    // configuration.
    //
    // `admin::init` also refuses these for free, because none of them exists in
    // `pg_roles` and it looks the role up before granting. Refusing them here
    // covers every caller rather than the one that happens to check.
    if matches!(
        role,
        "public" | "current_user" | "session_user" | "current_role"
    ) {
        return invalid("is a PostgreSQL role keyword rather than a role name");
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

    /// A legal KEK: 32 bytes, base64. Not a real one — it is `[7; 32]`, which
    /// no deployment would ever hold.
    fn kek_b64() -> String {
        BASE64_STANDARD.encode([7u8; DEK_BYTES])
    }

    #[test]
    fn the_blob_spine_refuses_to_start_without_a_root_a_key_and_a_label() {
        let full = |root: &str| {
            vec![
                (BLOB_ROOT_ENV, root.to_owned()),
                (BLOB_KEK_ENV, kek_b64()),
                (BLOB_KEK_ID_ENV, "kek-2026-07".to_owned()),
            ]
        };
        let lookup_owned = |pairs: Vec<(&str, String)>| {
            let owned: Vec<(String, String)> =
                pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
            move |key: &str| {
                owned
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| v.to_owned())
            }
        };

        let cfg =
            BlobConfig::from_lookup(lookup_owned(full("/var/lib/gridwork/blobs"))).expect("config");
        assert_eq!(cfg.root(), Path::new("/var/lib/gridwork/blobs"));
        assert_eq!(cfg.kek_id(), "kek-2026-07");
        assert_eq!(cfg.kek().expose_secret(), &[7u8; DEK_BYTES]);
        assert_eq!(
            cfg.pty_recording_retention_days(),
            DEFAULT_PTY_RECORDING_RETENTION_DAYS
        );

        // Each variable is required on its own: a deployment missing one must
        // be told which, not handed a default that silently stores blobs it
        // cannot protect or cannot find again.
        for missing in [BLOB_ROOT_ENV, BLOB_KEK_ENV, BLOB_KEK_ID_ENV] {
            let pairs: Vec<_> = full("/var/lib/gridwork/blobs")
                .into_iter()
                .filter(|(k, _)| *k != missing)
                .collect();
            let err = BlobConfig::from_lookup(lookup_owned(pairs))
                .expect_err(&format!("{missing} must be required"));
            assert!(err.to_string().contains(missing), "{err}");
        }

        // A relative root resolves against whatever directory the process
        // started in, which is not a place anybody chose.
        for bad_root in ["", "   ", "blobs", "./blobs", "../blobs"] {
            BlobConfig::from_lookup(lookup_owned(full(bad_root)))
                .expect_err(&format!("{bad_root:?} must be refused"));
        }
    }

    #[test]
    fn the_pty_recording_retention_age_is_configurable_and_bounded() {
        let configured = |days: &str| {
            let days = days.to_owned();
            BlobConfig::from_lookup(move |key| match key {
                BLOB_ROOT_ENV => Some("/var/lib/gridwork/blobs".to_owned()),
                BLOB_KEK_ENV => Some(kek_b64()),
                BLOB_KEK_ID_ENV => Some("kek-2026-07".to_owned()),
                PTY_RECORDING_RETENTION_DAYS_ENV => Some(days.clone()),
                _ => None,
            })
        };

        for days in ["1", "30", "365", " 45 "] {
            let config = configured(days).unwrap_or_else(|e| panic!("{days:?}: {e}"));
            assert_eq!(
                config.pty_recording_retention_days(),
                days.trim().parse::<i32>().expect("test integer")
            );
        }
        for days in ["", "0", "-1", "1.5", "forever", "2147483648"] {
            let error = configured(days).expect_err("invalid retention age must refuse");
            assert!(
                error.to_string().contains(PTY_RECORDING_RETENTION_DAYS_ENV),
                "{days:?}: {error}"
            );
        }
    }

    #[test]
    fn the_kek_must_decode_to_exactly_one_key() {
        let with_kek = |value: &str| {
            let owned = value.to_owned();
            move |key: &str| match key {
                BLOB_ROOT_ENV => Some("/var/lib/gridwork/blobs".to_owned()),
                BLOB_KEK_ENV => Some(owned.clone()),
                BLOB_KEK_ID_ENV => Some("kek-2026-07".to_owned()),
                _ => None,
            }
        };
        BlobConfig::from_lookup(with_kek(&kek_b64())).expect("32 bytes");
        // Whitespace around a value pasted out of a secret manager.
        BlobConfig::from_lookup(with_kek(&format!(" {}\n", kek_b64()))).expect("trimmed");

        for (why, value) in [
            ("not base64", "not base64 at all!".to_owned()),
            ("too short", BASE64_STANDARD.encode([7u8; DEK_BYTES - 1])),
            ("too long", BASE64_STANDARD.encode([7u8; DEK_BYTES + 1])),
            ("empty", String::new()),
        ] {
            let err = BlobConfig::from_lookup(with_kek(&value))
                .expect_err(&format!("{why} must be refused"));
            let message = err.to_string();
            assert!(message.contains(BLOB_KEK_ENV), "{why}: {message}");
            // The variable is named; its VALUE never is. A base64 error reports
            // offsets and lengths of the thing being decoded, and that thing is
            // a key.
            assert!(
                !message.contains(&value) || value.is_empty(),
                "{why}: {message}"
            );
        }
    }

    /// Every class variable, set — the ring `from_lookup` accepts.
    fn context_ring(overrides: &[(&str, &str)]) -> Vec<(String, String)> {
        let mut pairs: Vec<(String, String)> = Vec::new();
        for (index, class) in ContentClass::ALL.iter().enumerate() {
            let (kek_var, id_var) = context_kek_env(*class);
            // A distinct key per class, so a test that swaps them can tell.
            pairs.push((
                kek_var.to_owned(),
                BASE64_STANDARD.encode([index as u8 + 1; DEK_BYTES]),
            ));
            pairs.push((id_var.to_owned(), format!("kek-{}", class.as_str())));
        }
        for (name, value) in overrides {
            pairs.retain(|(k, _)| k != name);
            pairs.push(((*name).to_owned(), (*value).to_owned()));
        }
        pairs
    }

    fn lookup_pairs(pairs: Vec<(String, String)>) -> impl Fn(&str) -> Option<String> {
        move |key| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    #[test]
    fn the_context_key_ring_is_all_or_nothing() {
        let config =
            ContextBlobConfig::from_lookup(lookup_pairs(context_ring(&[]))).expect("full ring");
        // Every class answers, with its own key and its own label — asserted
        // over ALL so a new class extends this test by construction.
        let mut labels: Vec<String> = Vec::new();
        for class in ContentClass::ALL {
            let (kek, kek_id) = config.kek(class);
            assert_eq!(kek.expose_secret().len(), DEK_BYTES);
            assert!(!labels.contains(&kek_id.to_owned()), "{kek_id}");
            labels.push(kek_id.to_owned());
        }
        assert_eq!(labels.len(), ContentClass::ALL.len());

        // Fail closed at construction: each variable, removed alone, is a
        // refusal that names it. This is the "missing class key fails at
        // process start, not at first use" arm.
        for class in ContentClass::ALL {
            let (kek_var, id_var) = context_kek_env(class);
            for missing in [kek_var, id_var] {
                let pairs: Vec<(String, String)> = context_ring(&[])
                    .into_iter()
                    .filter(|(k, _)| k != missing)
                    .collect();
                let err = ContextBlobConfig::from_lookup(lookup_pairs(pairs))
                    .expect_err("a missing class variable must refuse");
                assert!(err.to_string().contains(missing), "{missing}: {err}");
            }
        }

        // Two classes wearing one label: refused, naming both the variable and
        // the class already holding it.
        let (_, private_id_var) = context_kek_env(ContentClass::Private);
        let err = ContextBlobConfig::from_lookup(lookup_pairs(context_ring(&[(
            private_id_var,
            "kek-conformance",
        )])))
        .expect_err("a shared label must refuse");
        let message = err.to_string();
        assert!(message.contains(private_id_var), "{message}");
        assert!(message.contains("conformance"), "{message}");
    }

    #[test]
    fn context_retention_windows_are_opt_in_per_bounded_class() {
        let base = |extra: Vec<(&str, String)>| {
            let mut pairs = vec![
                (BLOB_ROOT_ENV, "/var/lib/gridwork/blobs".to_owned()),
                (BLOB_KEK_ENV, kek_b64()),
                (BLOB_KEK_ID_ENV, "kek-2026-07".to_owned()),
            ];
            pairs.extend(extra);
            let owned: Vec<(String, String)> =
                pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect();
            lookup_pairs(owned)
        };

        // Nothing configured: no windows, and that is a complete answer — the
        // sweep retains every class.
        let config = BlobConfig::from_lookup(base(vec![])).expect("config");
        assert!(config.context_retention().is_empty());

        // One bounded class configured: exactly that window.
        let manifest_var =
            context_retention_env(RetentionClass::Manifest).expect("manifest is bounded");
        let config =
            BlobConfig::from_lookup(base(vec![(manifest_var, "45".to_owned())])).expect("config");
        assert_eq!(
            config.context_retention(),
            &[(RetentionClass::Manifest, 45)]
        );

        // A malformed window is a refusal naming its variable, same rule as
        // the pty window.
        for bad in ["0", "-1", "forever"] {
            let err = BlobConfig::from_lookup(base(vec![(manifest_var, bad.to_owned())]))
                .expect_err("invalid window must refuse");
            assert!(err.to_string().contains(manifest_var), "{bad}: {err}");
        }

        // Permanent has no variable at all: nothing to set is the design.
        assert_eq!(context_retention_env(RetentionClass::Permanent), None);
    }

    #[test]
    fn the_kek_label_stays_a_plain_short_name() {
        for good in ["k", "kek-2026-07", "prod.blob_kek", &"a".repeat(64)] {
            validate_kek_id(good).unwrap_or_else(|e| panic!("{good:?} should be legal: {e}"));
        }
        // Every rejection below would be copied verbatim into the header of
        // every container this key wraps.
        for bad in [
            "",
            "with space",
            "with/slash",
            "with\0null",
            "with\nnewline",
            "émoji",
            &"a".repeat(65),
        ] {
            validate_kek_id(bad).expect_err(&format!("{bad:?} must be refused"));
        }
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
            // The four that pass every character test above and are still not
            // role names. `public` is the one that matters: it is a legal bare
            // lowercase identifier by shape, and `GRANT ... TO public` hands the
            // whole runtime privilege matrix to every role in the cluster.
            "public",
            "current_user",
            "session_user",
            "current_role",
        ] {
            validate_role(bad).expect_err(&format!("{bad:?} must be refused"));
        }
    }

    /// The keyword rejection is about the keywords, not about a typo near them.
    ///
    /// Without this, widening the guard to anything that merely CONTAINS one of
    /// the four — a `contains`, a prefix test — would pass the arm above while
    /// locking out every legitimate role named for what it does.
    #[test]
    fn a_role_named_near_a_keyword_is_still_a_role() {
        for good in [
            "public_reader",
            "gwk_public",
            "publication",
            "current_user_shadow",
            "session_users",
        ] {
            validate_role(good)
                .unwrap_or_else(|e| panic!("{good:?} is a real identifier, not a keyword: {e}"));
        }
    }
}
