//! Content digests that are not necessarily content ADDRESSES.
//!
//! The distinction this module exists for: [`gwk_domain::BlobAddress`] names
//! bytes in the encrypted CAS, and holding one is a promise you can fetch them.
//! Most things Context hashes are not there and never will be — a file at a
//! git commit, a skill in the private overlay, an upstream package nobody
//! uploaded. Those need an identity, not a location.
//!
//! Reusing `BlobAddress` for them would force one of two bad outcomes: push
//! every hashed source into CAS so the type stays honest (inheriting key
//! custody, retention classes, and tombstoning for content that needed none of
//! it), or leave a type that says "fetchable" about things that are not.
//!
//! So: same validation, different meaning. The validation itself is shared with
//! `blob.rs` rather than re-implemented, because "lowercase 64-hex sha-256" is
//! one rule and two copies of it drift.
//!
//! Ruling R2 / fork F2.

/// The one digest scheme v1 mints.
///
/// Prefixed rather than bare for the same reason `BlobAddress` is: a second
/// hash function is a new prefix and a parser that rejects the old one where it
/// must, never a parser quietly loosened to accept both shapes.
pub const DIGEST_SCHEME: &str = "sha256:";

/// Why a digest did not parse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestError {
    /// Not prefixed `sha256:`.
    UnknownScheme,
    /// Not a lowercase 64-hex sha-256.
    MalformedDigest,
}

impl std::fmt::Display for DigestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownScheme => write!(f, "digest must start with {DIGEST_SCHEME}"),
            Self::MalformedDigest => f.write_str("digest must be a lowercase 64-hex sha-256"),
        }
    }
}

impl std::error::Error for DigestError {}

/// A validated `sha256:<lowercase 64-hex>` digest over content.
///
/// Construction is the only way in, so a value of this type is always a legal
/// digest — including one decoded off the wire, which is why `Deserialize` is
/// hand-written rather than derived.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct Digest(String);

impl Digest {
    /// Parse a full `sha256:<hex>` digest.
    pub fn parse(value: &str) -> Result<Self, DigestError> {
        let hex = value
            .strip_prefix(DIGEST_SCHEME)
            .ok_or(DigestError::UnknownScheme)?;
        if !gwk_domain::is_sha256_hex(hex) {
            return Err(DigestError::MalformedDigest);
        }
        Ok(Self(value.to_owned()))
    }

    /// Build a digest from a bare lowercase 64-hex hash.
    pub fn from_hex(hex: &str) -> Result<Self, DigestError> {
        if !gwk_domain::is_sha256_hex(hex) {
            return Err(DigestError::MalformedDigest);
        }
        Ok(Self(format!("{DIGEST_SCHEME}{hex}")))
    }

    /// The full scheme-prefixed form.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The bare hex, without the scheme prefix.
    pub fn hex(&self) -> &str {
        // Safe by construction: every path through `parse`/`from_hex` leaves
        // the prefix in place.
        &self.0[DIGEST_SCHEME.len()..]
    }
}

impl std::fmt::Display for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl<'de> serde::Deserialize<'de> for Digest {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        Self::parse(raw.as_ref()).map_err(serde::de::Error::custom)
    }
}

// The validated wire form is a string, not a structural object.
impl specta::Type for Digest {
    fn definition(types: &mut specta::Types) -> specta::datatype::DataType {
        <String as specta::Type>::definition(types)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    #[test]
    fn round_trips_through_both_constructors() {
        let parsed = Digest::parse(&format!("{DIGEST_SCHEME}{HEX}")).expect("valid");
        let built = Digest::from_hex(HEX).expect("valid");
        assert_eq!(parsed, built);
        assert_eq!(built.hex(), HEX);
        assert_eq!(built.as_str(), format!("{DIGEST_SCHEME}{HEX}"));
    }

    #[test]
    fn rejects_the_shapes_that_would_let_one_hash_hold_two_digests() {
        // Uppercase is the dangerous one: digests are compared byte-for-byte,
        // so accepting mixed case silently doubles every identity.
        assert_eq!(
            Digest::from_hex(&HEX.to_uppercase()),
            Err(DigestError::MalformedDigest)
        );
        assert_eq!(Digest::parse(HEX), Err(DigestError::UnknownScheme));
        assert_eq!(
            Digest::parse(&format!("sha512:{HEX}")),
            Err(DigestError::UnknownScheme)
        );
        assert_eq!(
            Digest::parse(&format!("{DIGEST_SCHEME}{}", &HEX[..63])),
            Err(DigestError::MalformedDigest)
        );
    }

    #[test]
    fn deserialize_validates_rather_than_trusting_the_wire() {
        let ok: Digest = serde_json::from_str(&format!("\"{DIGEST_SCHEME}{HEX}\"")).expect("valid");
        assert_eq!(ok.hex(), HEX);
        assert!(serde_json::from_str::<Digest>("\"not-a-digest\"").is_err());
        // A bare hex with no scheme is the most plausible bad input, since it
        // is what every other tool prints.
        assert!(serde_json::from_str::<Digest>(&format!("\"{HEX}\"")).is_err());
    }
}
