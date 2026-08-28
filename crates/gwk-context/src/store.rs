//! The Context truth-record read port.
//!
//! The compiler, the verifier, and the Explain/Compare evaluation all read the
//! four immutable truth records; none of them may care where the records live.
//! This trait is that seam, in the `gwk_domain::port` shape: engine-neutral,
//! digest- and id-addressed, no location in any signature. A backend proves
//! itself by conformance, not by being first.
//!
//! The WRITE side is deliberately absent. Truth records enter through the
//! kernel's lifecycle-event append path — one log, one writer discipline — so
//! a storage port that could write them would be a second door around it.
//!
//! The classified CAS half of Context storage lives beside the other storage
//! ports as [`gwk_domain::context`]: it needs nothing from this crate, and the
//! kernel-side adapter implements it without depending on one that is not
//! published. What THIS port serves is the typed record layer above it.

use gwk_domain::port::StorageError;

use crate::manifest::{
    FinalizationSupplement, ManifestId, ObservationSupplement, ReleaseSupplement, ResolvedManifest,
};

/// Read access to the four immutable Context truth records.
///
/// Every method answers from the record as it stands: `Ok(None)` means the
/// record does not exist, which for an immutable spine is a complete answer —
/// there is no "not yet visible" state a reader must poll past, because a
/// record either committed or it never happened.
pub trait ContextTruthStore {
    /// The resolved manifest, by its own id.
    fn manifest(
        &self,
        id: &ManifestId,
    ) -> impl Future<Output = Result<Option<ResolvedManifest>, StorageError>>;

    /// The manifest's one release supplement, if released.
    fn release(
        &self,
        manifest: &ManifestId,
    ) -> impl Future<Output = Result<Option<ReleaseSupplement>, StorageError>>;

    /// Every observation supplement, in observation-index order — the stable
    /// order the contract's own uniqueness constraint pins.
    fn observations(
        &self,
        manifest: &ManifestId,
    ) -> impl Future<Output = Result<Vec<ObservationSupplement>, StorageError>>;

    /// The manifest's one finalization supplement, if finalized.
    fn finalization(
        &self,
        manifest: &ManifestId,
    ) -> impl Future<Output = Result<Option<FinalizationSupplement>, StorageError>>;
}
