//! The checks themselves.
//!
//! Every rule here is re-derived from the canonical form as the compiler's
//! module documentation *states* it, never read off the compiler's code. That
//! distinction is the whole point of the crate: a verifier that imported the
//! producer's routine would agree with it by construction, including where the
//! routine is wrong, and would be evidence of nothing.
//!
//! The consequence to keep in mind when editing: if a rule below drifts from
//! the compiler's, the suite reds and one of the two is wrong. That is the
//! design working. Reaching for the compiler's implementation to "settle" the
//! disagreement discards the only independent reading there is.

use std::collections::BTreeSet;

use gwk_context::{
    Digest, FinalizationSupplement, ManifestId, ObservationSupplement, ParticipationState,
    ReleaseSupplement, ResolvedManifest,
};
use gwk_domain::EvidenceId;
use sha2::Digest as _;

/// The bare hex a manifest carries in `manifest_digest` while its own digest is
/// being computed.
///
/// Declared here rather than imported, because importing it would mean
/// depending on the crate that produced the value being checked. It is stated
/// as a contract in `gwk-context-compiler`'s module documentation; this is the
/// second, independent reading of that contract, and the digest arm reds if the
/// two ever disagree.
pub const MANIFEST_DIGEST_PLACEHOLDER_HEX: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// One failed property, naming what disagreed rather than that something did.
///
/// A verifier that answers only "rejected" moves the work of finding out to
/// whoever reads the answer, and CTX-11's whole concern is a package whose
/// parts are individually plausible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    /// The record's recorded digest is not the digest of the record.
    ManifestDigest {
        recorded: Digest,
        recomputed: Digest,
    },
    /// The digest scheme rejected a value this crate built. An internal fault,
    /// not a finding about the manifest.
    Encode(String),
    /// Participation rows are not in ascending digest order at this position.
    ParticipationOrder { position: usize },
    /// Two participation rows carry the same candidate digest.
    ParticipationDuplicate { digest: Digest },
    /// `source_count` disagrees with the number of rows actually admitted.
    SourceCount { recorded: u32, active: usize },
    /// Evidence ids are not in ascending order at this position.
    EvidenceOrder { position: usize },
    /// The same evidence id is cited twice by one record.
    EvidenceDuplicate { id: EvidenceId },
    /// A supplement names a manifest other than the one it was checked against.
    SupplementBinding {
        named: ManifestId,
        expected: ManifestId,
    },
    /// Observation indices are not `1..=n` in order without gaps.
    ObservationSequence { position: usize, index: u32 },
    /// The finalization's `observation_count` disagrees with the observations.
    ObservationCount { recorded: u32, observed: usize },
    /// A record cites evidence that resolves nowhere.
    EvidenceUnresolved { id: EvidenceId },
    /// A finalization claims a complete lifecycle without a release.
    LifecycleIncomplete,
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ManifestDigest {
                recorded,
                recomputed,
            } => write!(
                f,
                "manifest digest is {recorded:?} but the record hashes to {recomputed:?}"
            ),
            Self::Encode(detail) => write!(f, "verifier could not encode the record: {detail}"),
            Self::ParticipationOrder { position } => write!(
                f,
                "participation row {position} is not in ascending digest order"
            ),
            Self::ParticipationDuplicate { digest } => {
                write!(f, "participation digest {digest:?} appears twice")
            }
            Self::SourceCount { recorded, active } => write!(
                f,
                "source_count records {recorded} but {active} rows are active"
            ),
            Self::EvidenceOrder { position } => {
                write!(f, "evidence id {position} is not in ascending order")
            }
            Self::EvidenceDuplicate { id } => write!(f, "evidence id {id:?} is cited twice"),
            Self::SupplementBinding { named, expected } => write!(
                f,
                "supplement names manifest {named:?}, checked against {expected:?}"
            ),
            Self::ObservationSequence { position, index } => write!(
                f,
                "observation at position {position} carries index {index}"
            ),
            Self::ObservationCount { recorded, observed } => write!(
                f,
                "finalization records {recorded} observations, {observed} supplied"
            ),
            Self::EvidenceUnresolved { id } => {
                write!(f, "evidence id {id:?} resolves to nothing")
            }
            Self::LifecycleIncomplete => {
                f.write_str("finalization claims a complete lifecycle with no release")
            }
        }
    }
}

impl std::error::Error for VerifyError {}

/// The four truth records for one attempt, as a reader holds them.
///
/// Borrowed rather than owned: a verifier that took ownership would invite a
/// caller to hand over the only copy of what it is checking.
#[derive(Debug, Clone, Copy)]
pub struct Package<'a> {
    pub manifest: &'a ResolvedManifest,
    pub release: Option<&'a ReleaseSupplement>,
    /// In the order the store returned them; the sequence rule is checked, not
    /// assumed, so an unsorted slice is a finding rather than a panic.
    pub observations: &'a [ObservationSupplement],
    pub finalization: Option<&'a FinalizationSupplement>,
}

/// SHA-256 over the record serialized with `manifest_digest` set to the
/// placeholder, fields in declaration order, no whitespace.
///
/// Independently implemented from the stated canonical form. Note what is NOT
/// done here: the bytes come from serializing the STRUCT, never a
/// `serde_json::Value`, because a `Value` map's key order depends on which
/// serde_json features the final binary unified — a digest that changed with
/// the build would strand every manifest ever written.
pub fn manifest_digest(manifest: &ResolvedManifest) -> Result<Digest, VerifyError> {
    let mut preimage = manifest.clone();
    preimage.manifest_digest = Digest::from_hex(MANIFEST_DIGEST_PLACEHOLDER_HEX)
        .map_err(|error| VerifyError::Encode(error.to_string()))?;
    let bytes =
        serde_json::to_vec(&preimage).map_err(|error| VerifyError::Encode(error.to_string()))?;
    let raw: [u8; 32] = sha2::Sha256::digest(&bytes).into();
    Digest::from_hex(&hex_lower(&raw)).map_err(|error| VerifyError::Encode(error.to_string()))
}

fn hex_lower(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

/// Everything one resolved manifest claims about itself.
pub fn verify_manifest(manifest: &ResolvedManifest) -> Result<(), VerifyError> {
    let recomputed = manifest_digest(manifest)?;
    if recomputed != manifest.manifest_digest {
        return Err(VerifyError::ManifestDigest {
            recorded: manifest.manifest_digest.clone(),
            recomputed,
        });
    }

    let rows = manifest.participations.as_slice();
    for (position, pair) in rows.windows(2).enumerate() {
        match pair[0].digest.cmp(&pair[1].digest) {
            std::cmp::Ordering::Less => {}
            std::cmp::Ordering::Equal => {
                return Err(VerifyError::ParticipationDuplicate {
                    digest: pair[0].digest.clone(),
                });
            }
            std::cmp::Ordering::Greater => {
                return Err(VerifyError::ParticipationOrder {
                    position: position + 1,
                });
            }
        }
    }

    // `source_count` is the number of rows ADMITTED, not the number offered —
    // every offered candidate gets a row, and the ones that lost precedence or
    // a budget cut are recorded rather than dropped. Counting the active rows
    // is the one arm here that re-derives a scalar the compiler chose, which
    // is why it is worth having: the digest arm cannot see it, because a wrong
    // count is hashed as faithfully as a right one.
    let active = rows
        .iter()
        .filter(|row| row.state == ParticipationState::Active)
        .count();
    if manifest.source_count.value() as usize != active {
        return Err(VerifyError::SourceCount {
            recorded: manifest.source_count.value(),
            active,
        });
    }

    evidence_is_ordered_and_unique(manifest.evidence_ids.as_slice())
}

fn evidence_is_ordered_and_unique(ids: &[EvidenceId]) -> Result<(), VerifyError> {
    for (position, pair) in ids.windows(2).enumerate() {
        match pair[0].cmp(&pair[1]) {
            std::cmp::Ordering::Less => {}
            std::cmp::Ordering::Equal => {
                return Err(VerifyError::EvidenceDuplicate {
                    id: pair[0].clone(),
                });
            }
            std::cmp::Ordering::Greater => {
                return Err(VerifyError::EvidenceOrder {
                    position: position + 1,
                });
            }
        }
    }
    Ok(())
}

fn binds_to(named: &ManifestId, expected: &ManifestId) -> Result<(), VerifyError> {
    if named == expected {
        return Ok(());
    }
    Err(VerifyError::SupplementBinding {
        named: named.clone(),
        expected: expected.clone(),
    })
}

/// Every property the four records claim, checked against each other and
/// against the evidence a store actually holds.
///
/// `known_evidence` is what the caller's store can resolve. It is a parameter
/// rather than a lookup because the verifier must be runnable against a store
/// it does not own — including, in a test, one deliberately missing a row.
pub fn verify(
    package: &Package<'_>,
    known_evidence: &BTreeSet<EvidenceId>,
) -> Result<(), VerifyError> {
    verify_manifest(package.manifest)?;
    let manifest_id = &package.manifest.id;

    if let Some(release) = package.release {
        binds_to(&release.manifest_id, manifest_id)?;
        evidence_is_ordered_and_unique(release.evidence_ids.as_slice())?;
    }

    // Indices are `1..=n`, in order, no gaps. Checked by position rather than
    // by sorting first: a store that returned them out of order is itself the
    // finding, and sorting would erase it.
    for (position, observation) in package.observations.iter().enumerate() {
        binds_to(&observation.manifest_id, manifest_id)?;
        evidence_is_ordered_and_unique(observation.evidence_ids.as_slice())?;
        let expected = u32::try_from(position + 1).unwrap_or(u32::MAX);
        if observation.observation_index.value() != expected {
            return Err(VerifyError::ObservationSequence {
                position,
                index: observation.observation_index.value(),
            });
        }
    }

    if let Some(finalization) = package.finalization {
        binds_to(&finalization.manifest_id, manifest_id)?;
        evidence_is_ordered_and_unique(finalization.evidence_ids.as_slice())?;
        if finalization.observation_count.value() as usize != package.observations.len() {
            return Err(VerifyError::ObservationCount {
                recorded: finalization.observation_count.value(),
                observed: package.observations.len(),
            });
        }
        if finalization.lifecycle_complete && package.release.is_none() {
            return Err(VerifyError::LifecycleIncomplete);
        }
    }

    // Source linkage last, over every record's citations at once: an id that
    // resolves nowhere is the same finding wherever it was cited from.
    for id in cited_evidence(package) {
        if !known_evidence.contains(&id) {
            return Err(VerifyError::EvidenceUnresolved { id });
        }
    }

    Ok(())
}

/// Every evidence id any record in the package cites, in a stable order so the
/// first unresolved one is the same on every run.
pub fn cited_evidence(package: &Package<'_>) -> BTreeSet<EvidenceId> {
    let mut out: BTreeSet<EvidenceId> = package
        .manifest
        .evidence_ids
        .as_slice()
        .iter()
        .cloned()
        .collect();
    if let Some(release) = package.release {
        out.extend(release.evidence_ids.as_slice().iter().cloned());
    }
    for observation in package.observations {
        out.extend(observation.evidence_ids.as_slice().iter().cloned());
    }
    if let Some(finalization) = package.finalization {
        out.extend(finalization.evidence_ids.as_slice().iter().cloned());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_placeholder_is_sixty_four_zeros_and_a_legal_digest() {
        assert_eq!(MANIFEST_DIGEST_PLACEHOLDER_HEX.len(), 64);
        assert!(
            MANIFEST_DIGEST_PLACEHOLDER_HEX.bytes().all(|b| b == b'0'),
            "the placeholder is the all-zero digest"
        );
        assert!(Digest::from_hex(MANIFEST_DIGEST_PLACEHOLDER_HEX).is_ok());
    }

    #[test]
    fn hex_lower_is_lowercase_and_full_width() {
        let encoded = hex_lower(
            &[0x00, 0x0f, 0xa0, 0xff, 0x10, 0x01, 0x99, 0xab, 0xcd, 0xef]
                .iter()
                .copied()
                .cycle()
                .take(32)
                .collect::<Vec<u8>>()
                .try_into()
                .expect("32 bytes"),
        );
        assert_eq!(encoded.len(), 64);
        assert_eq!(encoded, encoded.to_lowercase());
        assert!(encoded.bytes().all(|b| b.is_ascii_hexdigit()));
    }
}
