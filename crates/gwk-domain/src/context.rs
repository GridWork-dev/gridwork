//! The Context CAS metadata contract: classification classes and the storage
//! port over them.
//!
//! Context blobs ride the same encrypted CAS as every other blob (R17: the v1
//! container bytes are untouched — these are metadata-layer concerns BESIDE the
//! blob, exactly how evidence pinning was added without touching the container
//! format). What Context adds is classification, and each class axis is a
//! closed set enforced three times over: here as an enum, in the contract DDL
//! as a CHECK, and by the token-parity gate that holds the two to each other.
//!
//! The three axes are orthogonal and none is derivable from another:
//!
//! * [`ContentClass`] is the KEK domain (R19): one key-encryption key per
//!   content class, so a public/private seam false-negative becomes a contained
//!   compromise rather than a full private-content one. The class decides which
//!   KEK seals a blob's DEK; a blob sealed under one class's KEK fails
//!   authentication under another's.
//! * [`RedactionClass`] records what treatment the plaintext received before it
//!   was sealed. The classification travels beside the blob so an audit can ask
//!   it without opening anything; the redaction BEHAVIOUR itself belongs to the
//!   runtimes that produce the bytes, not to storage.
//! * [`RetentionClass`] is D4's "retention by content class" given a mechanism
//!   (R20): a first-class column the sweep keys on, not another hardcoded
//!   branch in backend SQL. The class set is contract; the per-class windows
//!   are deployment policy and live in backend configuration — a class with no
//!   configured window is retained, so an unconfigured deployment fails safe
//!   toward keeping bytes.
//!
//! Evidence pinning is reused as-is (R21): Context blobs are pinned through the
//! same (digest, evidence id) set as every other blob, and a pin overrides
//! retention expiry unconditionally.
//!
//! The port stays engine-neutral like its siblings in [`crate::port`]: no
//! GridWork policy value (a window length, a key label, a deployment path)
//! appears in a signature here. A backend proves itself by conformance.

use crate::blob::{BlobAddress, BlobDescriptor};
use crate::ids::{EvidenceId, Timestamp};
use crate::port::BlobError;

/// One closed classification axis: the enum, its exhaustive `ALL`, and the
/// token each variant carries in the DDL and in configuration.
///
/// The same shape as `protocol_versions!` in [`crate::protocol`]: one list
/// yields the enum, the arity, and both string directions, so no count
/// assertion over it can be the `[Self; N].len()` tautology and no token can
/// drift from its variant — there is exactly one place either is written.
macro_rules! context_class {
    (
        $(#[$doc:meta])*
        pub enum $name:ident {
            $( $(#[$vdoc:meta])* $variant:ident => $token:literal ),+ $(,)?
        }
    ) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum $name {
            $( $(#[$vdoc])* $variant, )+
        }

        impl $name {
            /// Every variant. The arity is derived from the list itself, so a
            /// count over this can fail when the list grows.
            pub const ALL: [Self; [$(stringify!($variant)),+].len()] =
                [$(Self::$variant),+];

            /// The token this variant carries in the DDL CHECK and in
            /// configuration.
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $token,)+ }
            }

            /// The exact inverse of [`Self::as_str`]: the token or nothing.
            pub fn parse(value: &str) -> Option<Self> {
                match value { $($token => Some(Self::$variant),)+ _ => None }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

context_class! {
    /// The KEK domain a blob is sealed under (R19).
    ///
    /// The split is the public/private seam (F18): conformance fixtures and
    /// real content share one physical store and never share a key, so a blob
    /// that crosses the seam is unreadable rather than quietly exposed.
    pub enum ContentClass {
        /// Public conformance-fixture content: synthetic, reviewable, shipped.
        Conformance => "conformance",
        /// Real deployment content. The class name is contract; everything the
        /// class protects is not.
        Private => "private",
    }
}

context_class! {
    /// What treatment the plaintext received before sealing.
    pub enum RedactionClass {
        /// Stored as produced; nothing was removed.
        None => "none",
        /// The redaction pass ran: the stored bytes are the reconstructable
        /// redacted form, not the original.
        Redacted => "redacted",
    }
}

context_class! {
    /// The retention family the sweep keys on (R20).
    ///
    /// Bounded families beyond these arrive additively (each is a contract
    /// change with its step); 8D's memory pages join when their writer exists.
    pub enum RetentionClass {
        /// Never reclaimed by age.
        Permanent => "permanent",
        /// Resolved-manifest content.
        Manifest => "manifest",
        /// Release-supplement content.
        Release => "release",
        /// Observation-supplement content.
        Observation => "observation",
    }
}

/// The complete classification of one Context blob — what the metadata row
/// carries beside the blob, never inside the container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlobClasses {
    pub content: ContentClass,
    pub redaction: RedactionClass,
    pub retention: RetentionClass,
}

/// What the store knows about one classified Context blob.
///
/// Deliberately byte-free: the record is classification and accounting, and a
/// contract test holds the backing table to the same property — no column of
/// it can carry reconstructable content.
///
/// The classification half is always present — the metadata row outlives the
/// bytes on purpose, the way an evidence row outlives a swept recording. The
/// blob half is `None` once the CAS no longer holds the row (or never did: a
/// crash between the classification claim and the bytes, which a retried put
/// completes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextBlobRecord {
    pub digest: BlobAddress,
    pub classes: BlobClasses,
    /// When the classification was claimed — the instant retention counts from.
    pub created_at: Timestamp,
    /// The CAS row, while it exists. Its `kek_id` is the class KEK's nonsecret
    /// label; `tombstoned`/`pinned` answer the audit questions directly.
    pub blob: Option<BlobDescriptor>,
}

/// Why a Context CAS operation was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextCasError {
    /// The digest is already classified and the classification disagrees.
    ///
    /// Content addressing makes this unrepresentable any other way: one digest
    /// is one blob, a blob is sealed under exactly one class KEK, and
    /// re-presenting the same bytes under a different classification is a
    /// caller error — not a second blob, and never a reclassification.
    ClassMismatch {
        digest: BlobAddress,
        stored: BlobClasses,
        requested: BlobClasses,
    },
    /// A read named a content class the digest is not classified under. The
    /// refusal happens at the metadata layer; the per-class KEK is the
    /// backstop that makes the bytes unreadable even without it.
    WrongContentClass {
        digest: BlobAddress,
        stored: ContentClass,
        requested: ContentClass,
    },
    /// No classified Context blob at that digest.
    NotFound,
    /// The bytes already exist in the CAS sealed under a key domain that is
    /// not any Context class's — a content collision with a kernel-internal
    /// blob (a checkpoint snapshot, a payload blob). Classifying it would
    /// promise a class KEK that cannot open it, so the write is refused
    /// before any classification is claimed. The label is nonsecret.
    ForeignKeyDomain {
        digest: BlobAddress,
        stored_kek_id: String,
    },
    /// The blob layer refused: integrity, tombstone, pin, storage.
    Blob(BlobError),
}

impl std::fmt::Display for ContextCasError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClassMismatch {
                digest,
                stored,
                requested,
            } => write!(
                f,
                "{digest} is already classified {}/{}/{}; this write declares {}/{}/{}",
                stored.content,
                stored.redaction,
                stored.retention,
                requested.content,
                requested.redaction,
                requested.retention
            ),
            Self::WrongContentClass {
                digest,
                stored,
                requested,
            } => write!(
                f,
                "{digest} is {stored}-class content and was requested as {requested}"
            ),
            Self::NotFound => f.write_str("no classified context blob at that digest"),
            Self::ForeignKeyDomain {
                digest,
                stored_kek_id,
            } => write!(
                f,
                "{digest} already exists sealed under key domain {stored_kek_id:?}, which is not \
                 a context class's"
            ),
            Self::Blob(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ContextCasError {}

impl From<BlobError> for ContextCasError {
    fn from(error: BlobError) -> Self {
        Self::Blob(error)
    }
}

/// The Context content-addressed store: classified put/get over the encrypted
/// CAS, plus the evidence linkage retention answers to.
///
/// Everything is digest-addressed — no operation takes a path or any other
/// location, so the validated address alone determines what is touched, as in
/// [`crate::port::BlobStore`]. Objects here are bounded (manifests,
/// supplements, rendered context), so reads return the whole plaintext; the
/// chunked-upload surface stays a backend concern behind `put`.
pub trait ContextCasStore {
    /// Store `plaintext` under `classes`.
    ///
    /// Returns the record and whether an identical classified blob already
    /// existed — a dedup hit writes nothing new. The same bytes under a
    /// DIFFERENT classification are refused as [`ContextCasError::ClassMismatch`]:
    /// the classification is claimed atomically before any byte lands, so two
    /// racing writers cannot split it.
    fn put(
        &self,
        classes: BlobClasses,
        media_type: String,
        plaintext: &[u8],
    ) -> impl Future<Output = Result<(ContextBlobRecord, bool), ContextCasError>>;

    /// Read the whole plaintext of a blob classified under `content`.
    ///
    /// A digest classified under any other content class is refused before a
    /// byte is read; the per-class KEK makes the same answer hold even if the
    /// metadata were bypassed, because the wrapped DEK fails authentication
    /// under the wrong class key.
    fn get(
        &self,
        digest: &BlobAddress,
        content: ContentClass,
    ) -> impl Future<Output = Result<Vec<u8>, ContextCasError>>;

    /// The classification record, or `None` when the digest was never
    /// classified. Class-blind on purpose: describing a blob reveals its
    /// classification and nothing the classification protects.
    fn describe(
        &self,
        digest: &BlobAddress,
    ) -> impl Future<Output = Result<Option<ContextBlobRecord>, ContextCasError>>;

    /// Pin as evidence (R21: the same pin set every blob answers to), blocking
    /// sweep — retention expiry included — until every pin is released.
    fn pin(
        &self,
        digest: &BlobAddress,
        evidence: &EvidenceId,
    ) -> impl Future<Output = Result<(), ContextCasError>>;

    fn unpin(
        &self,
        digest: &BlobAddress,
        evidence: &EvidenceId,
    ) -> impl Future<Output = Result<(), ContextCasError>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The token and the variant are written once in the macro list, so the
    /// only drift left is a variant added to one enum's list and not its DDL
    /// CHECK — which the xtask token-parity gate owns. What this test owns is
    /// the string round trip itself, for every variant of every axis.
    #[test]
    fn every_class_token_round_trips_and_none_collides() {
        fn round_trip<T: Copy + PartialEq + std::fmt::Debug>(
            all: &[T],
            as_str: impl Fn(T) -> &'static str,
            parse: impl Fn(&str) -> Option<T>,
        ) {
            let mut seen: Vec<&str> = Vec::new();
            for value in all {
                let token = as_str(*value);
                assert_eq!(parse(token), Some(*value), "{token}");
                assert!(!seen.contains(&token), "duplicate token {token}");
                seen.push(token);
            }
            assert_eq!(parse("bogus"), None);
            // A count over the derived arity can fail on growth; asserted
            // non-zero so the loop above provably ran over something.
            assert!(!seen.is_empty());
        }
        round_trip(
            &ContentClass::ALL,
            ContentClass::as_str,
            ContentClass::parse,
        );
        round_trip(
            &RedactionClass::ALL,
            RedactionClass::as_str,
            RedactionClass::parse,
        );
        round_trip(
            &RetentionClass::ALL,
            RetentionClass::as_str,
            RetentionClass::parse,
        );
        // The arities, derived from the lists: growth in any axis moves one of
        // these and forces the DDL CHECK plus the parity gate to move with it.
        assert_eq!(ContentClass::ALL.len(), 2);
        assert_eq!(RedactionClass::ALL.len(), 2);
        assert_eq!(RetentionClass::ALL.len(), 4);
    }

    #[test]
    fn refusals_name_the_digest_and_both_sides() {
        let digest = BlobAddress::from_digest(&"a".repeat(64)).expect("digest");
        let stored = BlobClasses {
            content: ContentClass::Private,
            redaction: RedactionClass::Redacted,
            retention: RetentionClass::Manifest,
        };
        let requested = BlobClasses {
            content: ContentClass::Conformance,
            redaction: RedactionClass::None,
            retention: RetentionClass::Permanent,
        };
        let message = ContextCasError::ClassMismatch {
            digest: digest.clone(),
            stored,
            requested,
        }
        .to_string();
        for token in [
            "private/redacted/manifest",
            "conformance/none/permanent",
            digest.as_str(),
        ] {
            assert!(message.contains(token), "{message}");
        }

        let message = ContextCasError::WrongContentClass {
            digest: digest.clone(),
            stored: ContentClass::Private,
            requested: ContentClass::Conformance,
        }
        .to_string();
        assert!(
            message.contains("private-class content and was requested as conformance"),
            "{message}"
        );
    }
}
