//! Embeds the backend-neutral contract DDL into a Rust constant the kernel
//! can carry.
//!
//! `schema/0001_contract.sql` sits at the repo root, outside every package
//! directory, and `include_str!` cannot reach it from a crate that will be
//! published — `cargo package` copies only files under the package root, so a
//! `../../../` reach breaks at publish time, not at desk time. Copying the
//! bytes in through the generator keeps ONE authored file (the root SQL, which
//! CI applies to a real PostgreSQL) and makes the copy a checked artifact
//! rather than a second source.

use sha2::{Digest, Sha256};

/// Where the generated copy lands, relative to the repo root.
pub const GENERATED_PATH: &str = "crates/gwk-kernel/src/contract_sql.rs";

/// Lowercase-hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut acc, b| {
            use std::fmt::Write as _;
            let _ = write!(acc, "{b:02x}");
            acc
        })
}

/// The generated module for `sql`, byte for byte what belongs on disk.
pub fn contract_sql_rs(sql: &str) -> String {
    // The SQL is emitted as an `r#"..."#` literal. It contains no `"` today
    // and PostgreSQL only needs double quotes for identifiers this schema
    // never uses, but a future edit could add one — fail loudly here rather
    // than emit a file that does not parse.
    assert!(
        !sql.contains("\"#"),
        "schema/0001_contract.sql contains `\"#`, which closes the raw string \
         literal this generator emits — widen the hash fence before adding it"
    );
    // rustc normalizes CR and CRLF to LF inside a string literal, raw ones
    // included. A CRLF file would therefore digest one byte sequence here and
    // compile to a different one — breaking the single invariant this module
    // exists to hold, that CONTRACT_SQL_SHA256 describes CONTRACT_SQL.
    assert!(
        !sql.contains('\r'),
        "schema/0001_contract.sql has CR line endings; rustc would normalize them away inside \
         the emitted literal and the digest would stop describing CONTRACT_SQL"
    );
    let digest = sha256_hex(sql.as_bytes());
    format!(
        "// The backend-neutral contract DDL, embedded from schema/0001_contract.sql\n\
         // by `cargo run -p xtask -- contract`.\n\
         // DO NOT EDIT — regenerate instead; CI diffs this file against the source.\n\
         \n\
         /// Every byte of `schema/0001_contract.sql`. The script wraps itself in\n\
         /// `BEGIN`/`COMMIT`, so it is applied whole and never inside another\n\
         /// transaction.\n\
         pub const CONTRACT_SQL: &str = r#\"{sql}\"#;\n\
         \n\
         /// SHA-256 of [`CONTRACT_SQL`], lowercase hex. Initialization records this\n\
         /// in `gwk_internal.schema_fingerprint`, so a later boot can tell \"this\n\
         /// database carries MY contract\" from \"this database carries another\".\n\
         // Pre-wrapped: `cargo fmt --all` formats generated files too, and an\n\
         // unwrapped 64-hex line lands past 100 columns — the generator and\n\
         // rustfmt would then fight, showing up as permanent contract drift.\n\
         pub const CONTRACT_SQL_SHA256: &str =\n    \"{digest}\";\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_lowercase_hex_of_the_bytes() {
        // The published SHA-256 of the empty input.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(sha256_hex(b"abc").len(), 64);
    }

    #[test]
    fn generated_module_carries_the_sql_and_its_digest() {
        let out = contract_sql_rs("SELECT 1;\n");
        assert!(out.contains("pub const CONTRACT_SQL: &str = r#\"SELECT 1;\n\"#;"));
        assert!(out.contains(&format!(
            "pub const CONTRACT_SQL_SHA256: &str =\n    \"{}\";",
            sha256_hex(b"SELECT 1;\n")
        )));
    }

    #[test]
    #[should_panic(expected = "closes the raw string literal")]
    fn a_hash_fence_collision_fails_the_generator() {
        contract_sql_rs("SELECT '\"#';\n");
    }

    #[test]
    #[should_panic(expected = "CR line endings")]
    fn carriage_returns_fail_the_generator() {
        contract_sql_rs("SELECT 1;\r\n");
    }
}
