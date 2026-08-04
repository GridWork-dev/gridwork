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
/// The key a rotation is moving TO, same encoding as [`BLOB_KEK_ENV`].
///
/// Its own variable, read only by `gw admin blob rotate`, because a rotation is
/// the one operation that needs both keys at once while every other process
/// needs exactly one. Carrying the incoming key in the running key's variable
/// would leave nothing able to say which of the two it was holding.
pub const BLOB_KEK_NEXT_ENV: &str = "GWK_BLOB_KEK_NEXT";

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

        Ok(Self {
            root,
            kek: SecretBox::new(kek),
            kek_id,
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
        })
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
    let invalid = |why: &str| {
        Err(KernelError::Config(format!(
            "{BLOB_KEK_ID_ENV} {why}: expected 1..={MAX_KEK_ID_BYTES} bytes matching \
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
        ] {
            validate_role(bad).expect_err(&format!("{bad:?} must be refused"));
        }
    }
}
