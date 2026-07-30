//! Content-addressed blob contracts.
//!
//! Anything larger than the inline payload bound
//! ([`INLINE_PAYLOAD_MAX_BYTES`](crate::envelope::INLINE_PAYLOAD_MAX_BYTES))
//! leaves the event log and becomes a blob. The address is a digest over the
//! PLAINTEXT — encryption is a storage concern (ADR 0003), so two deployments
//! with different KEKs still agree on what a blob IS.
//!
//! The storage port itself lives beside `EventStore` in [`crate::port`].

use crate::ids::{ByteCount, Timestamp};

/// The one address scheme v1 mints. Parsing is exact: a future scheme is a new
/// prefix, never a loosened parser.
pub const BLOB_ADDRESS_SCHEME: &str = "sha256:";

/// Hex length of a SHA-256 digest.
pub const SHA256_HEX_LEN: usize = 64;

/// Plaintext chunk size for the streaming AEAD (ADR 0003). One chunk plus its
/// base64 expansion stays well inside the wire frame bound.
pub const BLOB_CHUNK_BYTES: usize = 1024 * 1024;

/// True iff `value` is a lowercase 64-hex SHA-256 digest — the exact form every
/// digest field in this contract carries (blob addresses, the archive manifest
/// digest, checkpoint projection hashes).
///
/// Lowercase only: an address is compared byte-for-byte, so accepting mixed
/// case would let one blob hold two addresses.
pub fn is_sha256_hex(value: &str) -> bool {
    value.len() == SHA256_HEX_LEN
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Why an address did not parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobAddressError {
    /// Not prefixed `sha256:`.
    UnknownScheme,
    /// The digest is not a lowercase 64-hex SHA-256.
    MalformedDigest,
}

impl std::fmt::Display for BlobAddressError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownScheme => write!(f, "blob address must start with {BLOB_ADDRESS_SCHEME}"),
            Self::MalformedDigest => {
                f.write_str("blob address digest must be a lowercase 64-hex sha-256")
            }
        }
    }
}

impl std::error::Error for BlobAddressError {}

/// A validated `sha256:<lowercase 64-hex>` content address over plaintext.
///
/// Construction is the ONLY way in, so a value of this type is always a legal
/// address — including one decoded off the wire.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct BlobAddress(String);

impl BlobAddress {
    /// Parse a full `sha256:<hex>` address.
    pub fn parse(value: &str) -> Result<Self, BlobAddressError> {
        let digest = value
            .strip_prefix(BLOB_ADDRESS_SCHEME)
            .ok_or(BlobAddressError::UnknownScheme)?;
        if !is_sha256_hex(digest) {
            return Err(BlobAddressError::MalformedDigest);
        }
        Ok(Self(value.to_owned()))
    }

    /// Build an address from a bare lowercase 64-hex digest.
    pub fn from_digest(digest: &str) -> Result<Self, BlobAddressError> {
        if !is_sha256_hex(digest) {
            return Err(BlobAddressError::MalformedDigest);
        }
        Ok(Self(format!("{BLOB_ADDRESS_SCHEME}{digest}")))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The digest without its scheme prefix — what a sharded storage path is
    /// derived from. Never a caller-supplied path (ADR 0003).
    pub fn digest_hex(&self) -> &str {
        // The prefix is guaranteed by construction.
        &self.0[BLOB_ADDRESS_SCHEME.len()..]
    }
}

impl std::fmt::Display for BlobAddress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for BlobAddress {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

// The WIRE form is a string, so the exported TS type is `string`.
impl specta::Type for BlobAddress {
    fn definition(types: &mut specta::Types) -> specta::datatype::DataType {
        <String as specta::Type>::definition(types)
    }
}

/// What the store knows about one committed blob.
///
/// Deduplication requires digest, size, AND media type to match (ADR 0003), so
/// all three are part of the identity a caller compares — not just the address.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct BlobDescriptor {
    pub address: BlobAddress,
    pub media_type: String,
    pub byte_size: ByteCount,
    /// Nonsecret label of the KEK whose wrapped DEK opens this blob. The KEK
    /// itself never appears in a persisted record or on the wire.
    pub kek_id: String,
    pub created_at: Timestamp,
    /// Pinned as evidence: excluded from retention sweeps until unpinned.
    pub pinned: bool,
    /// Crypto-shredded: the wrapped DEK is gone and reads fail permanently,
    /// even while ciphertext cleanup is still pending.
    pub tombstoned: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_parses_only_the_exact_v1_shape() {
        let hex = "a".repeat(SHA256_HEX_LEN);
        let addr = BlobAddress::parse(&format!("sha256:{hex}")).expect("valid address");
        assert_eq!(addr.digest_hex(), hex);

        for bad in [
            "sha256:ABC",                           // too short
            &format!("sha256:{}", "A".repeat(64)),  // uppercase hex
            &format!("sha256:{}", "a".repeat(63)),  // one nibble short
            &format!("sha256:{}", "a".repeat(65)),  // one nibble long
            &format!("sha512:{}", "a".repeat(64)),  // another scheme
            &"a".repeat(64),                        // bare digest, no scheme
            &format!("sha256:{}", "g".repeat(64)),  // not hex
            &format!(" sha256:{}", "a".repeat(64)), // leading space
            &format!("sha256:{} ", "a".repeat(64)), // trailing space
        ] {
            assert!(BlobAddress::parse(bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn address_round_trips_and_rejects_bad_input_at_decode_time() {
        let addr = BlobAddress::from_digest(&"0".repeat(SHA256_HEX_LEN)).expect("valid digest");
        let json = serde_json::to_string(&addr).expect("serialize");
        assert_eq!(json, format!("\"sha256:{}\"", "0".repeat(64)));
        let back: BlobAddress = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, addr);
        // Validation is not a construction-time-only courtesy: the wire path
        // enforces it too, or a malformed address walks in through serde.
        assert!(serde_json::from_str::<BlobAddress>("\"sha256:nope\"").is_err());
    }

    #[test]
    fn digest_predicate_is_lowercase_and_exact_length() {
        assert!(is_sha256_hex(&"0123456789abcdef".repeat(4)));
        assert!(!is_sha256_hex(&"0123456789ABCDEF".repeat(4)));
        assert!(!is_sha256_hex(""));
        assert!(!is_sha256_hex(&"a".repeat(63)));
    }
}
