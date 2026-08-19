// The contract's migration steps, embedded from schema/steps/*.sql
// by `cargo run -p xtask -- contract`.
// DO NOT EDIT — regenerate instead; CI diffs this file against the source.

/// One authored migration step: the DDL that carries a database from the
/// contract digest in `base` to the one in `result`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step {
    /// The step's file name under `schema/steps/`. It is the identity a
    /// receipt and a ledger row will name, which is why it is carried rather
    /// than an index into [`CONTRACT_STEPS`] — an index moves the moment a
    /// step is inserted.
    pub id: &'static str,
    /// The contract digest a database must already carry to take this step.
    pub base: &'static str,
    /// The contract digest a database carries once this step has been applied.
    pub result: &'static str,
    /// Every byte of the step's file, header comments included.
    pub sql: &'static str,
}

/// Every step under `schema/steps/`, in file-name order and no other order.
/// The order they are APPLIED in is a property of the digests, not of this
/// slice, and `gwk_kernel::migrate` is what reads it out of them.
// Laid out by the generator: rustfmt formats one element differently from
// two, so no emitted shape survives both. See xtask/src/steps.rs.
#[rustfmt::skip]
pub const CONTRACT_STEPS: &[Step] = &[];
