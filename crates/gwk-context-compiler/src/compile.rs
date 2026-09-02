//! The deterministic compiler: one immutable [`ResolvedManifest`] per spawn
//! attempt, from typed inputs, with the same bytes for the same inputs
//! regardless of the order anything arrived in.
//!
//! ## Inputs are values, not reads
//!
//! ADR-0032 D3 fixes the dispatch order: route resolution, then authority
//! resolution, then context compilation. Both upstream outputs reach this
//! crate as immutable typed values — a [`Route`] and an [`Authority`] — never
//! as live kernel reads, and the candidates the compiler chooses among arrive
//! the same way: the caller loads them through the Task 7 storage ports and
//! hands over [`Candidate`] values. The compiler performs no I/O, which is
//! what makes "deterministic" a property of the function rather than a
//! promise about the environment.
//!
//! ## What one compile decides
//!
//! 1. Every candidate gets exactly one participation row — the ones upstream
//!    already ruled out included, because "why was this not used" is answered
//!    from the record, not reconstructed (SPEC clause 3). The equality
//!    candidates-in equals rows-out is asserted, counted before compared.
//! 2. Ready candidates speaking to the same [`Candidate::slot`] are resolved
//!    by the D5 precedence order through [`resolve`]. A higher tier wins; the
//!    losers are `PrecedenceLoss`; an equal-tier disagreement fails the whole
//!    compile closed with [`CompileError::PrecedenceConflict`].
//! 3. Winners are admitted against the byte budget in one fixed order —
//!    highest authority first, then slot, then digest — and a winner that
//!    does not fit is `BudgetCut`. A `Security`-tier winner that does not fit
//!    fails the compile instead: dropping a security constraint to make room
//!    is widening authority by another name.
//! 4. Each active candidate's claimed tools are intersected with the
//!    authority's grant (D3: context narrows, never widens). The result is
//!    per candidate, and asserted a subset of the input on every path.
//! 5. The manifest digest is computed over the finished record, and source
//!    attribution is re-derived from that record alone (R12).
//!
//! ## The canonical form
//!
//! Two rules make the output byte-identical under input permutation. The
//! verifier — a separate crate by R15 — recomputes both without this code, so
//! they are stated here as the contract rather than left to be read off the
//! implementation:
//!
//! - **Participations are ordered by digest**, ascending, and every candidate
//!   digest is unique — a duplicate is refused rather than merged. Evidence
//!   ids are sorted and deduplicated.
//! - **`manifest_digest` is SHA-256 over the manifest serialized as JSON with
//!   `manifest_digest` set to the all-zero digest**
//!   ([`MANIFEST_DIGEST_PLACEHOLDER_HEX`] under the `sha256:` scheme), fields
//!   in the type's own declaration order, no whitespace — the bytes
//!   `serde_json::to_vec` emits for the struct. Serializing the STRUCT rather
//!   than a `serde_json::Value` is load-bearing: a `Value` map's key order
//!   depends on which serde_json features the final binary unified, and a
//!   digest that changed with the build would strand every manifest.

use std::collections::{BTreeMap, BTreeSet};

use gwk_context::{
    AttributionPart, CONTEXT_ID_MAX_BYTES, ContentClass, ContextAttribution, Contribution, Digest,
    EvidenceRefs, ManifestId, Participation, ParticipationReason, ParticipationRecords,
    ParticipationState, PrecedenceConflict, PrecedenceTier, RecordCount, ResolvedManifest,
    TruthRecordError,
};
use gwk_domain::{AttemptId, ByteCount, EvidenceId, Timestamp};
use sha2::Digest as _;

use crate::precedence::resolve;

/// This build's own attribution part: crate name and version.
///
/// The only thing the attribution says about who compiled — a build identity,
/// never an actor.
pub const COMPILER: &str = concat!("gwk-context-compiler/", env!("CARGO_PKG_VERSION"));

/// The bare hex a manifest carries in `manifest_digest` while its digest is
/// being computed: 64 zeros. Part of the canonical form (module docs).
pub const MANIFEST_DIGEST_PLACEHOLDER_HEX: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// The route the manifest is bound to — by digest only.
///
/// Route resolution's output is upstream and immutable (D3); the compiler
/// records which route it compiled against and changes nothing about it —
/// engine, role, lane, isolation, and permission profile are the route's, and
/// context may never silently change them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub digest: Digest,
}

/// The authority in force at resolution.
///
/// `tools` is the grant every candidate's claim is intersected with. It is
/// the ceiling; nothing the compiler emits exceeds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authority {
    pub digest: Digest,
    pub tools: BTreeSet<String>,
}

/// What upstream already decided about a candidate before compilation.
///
/// Trust state (D5: quarantined, then verified or rejected for one digest),
/// route eligibility (8C's decision), and authority's verdict all arrive
/// settled. The compiler does not re-decide any of them; it records them, so
/// the participation record is complete rather than only covering the
/// candidates that got as far as precedence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Standing {
    /// Verified, eligible, permitted: competes on precedence and budget.
    Ready,
    /// The route, role, or capability does not admit it.
    NotEligible,
    /// Authority resolution excluded it — upstream, never overridable here.
    Denied,
    /// Third-party material still in its initial quarantined trust state.
    Quarantined,
    /// Reviewed and rejected for this exact digest.
    Rejected,
    /// The verified pin does not match the digest actually found.
    PinDrift,
    /// The source could not be read at all.
    Unreadable,
}

impl Standing {
    /// The participation row an upstream verdict becomes; `None` for the one
    /// standing that competes.
    fn verdict(self) -> Option<(ParticipationState, ParticipationReason)> {
        use ParticipationReason as R;
        use ParticipationState as S;
        match self {
            Self::Ready => None,
            Self::NotEligible => Some((S::Excluded, R::NotEligible)),
            Self::Denied => Some((S::Excluded, R::PermissionDenied)),
            Self::Quarantined => Some((S::Unavailable, R::Quarantined)),
            Self::Rejected => Some((S::Unavailable, R::Rejected)),
            Self::PinDrift => Some((S::Unavailable, R::PinDrift)),
            Self::Unreadable => Some((S::Unavailable, R::Unavailable)),
        }
    }
}

/// One source offered to the compiler.
///
/// `slot` is the decision the candidate speaks to — the skill name, the
/// setting, the instruction — and is what precedence resolves per: two Ready
/// candidates in one slot are two answers to one question. It is grouping
/// only; no truth record carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// What the candidate is, by content. Unique across one compile.
    pub digest: Digest,
    /// Which side of the public/private seam the source came from.
    ///
    /// Supplied by whoever offers the candidate and carried through to its
    /// participation record unchanged — the compiler classifies nothing. It
    /// cannot: the class is a property of where the bytes came from, and the
    /// compiler sees a digest and a cost. A compiler-invented class would be a
    /// guess recorded as a fact, which is the failure mode the record exists
    /// to prevent.
    pub class: ContentClass,
    pub slot: String,
    pub tier: PrecedenceTier,
    /// What admitting it costs against the request's budget.
    pub bytes: ByteCount,
    /// Claimed, never granted — E4's `allowed-tools` evidence. Intersected
    /// with [`Authority::tools`]; never a grant on its own.
    pub claimed_tools: Vec<String>,
    pub standing: Standing,
}

/// The attempt-scoped half of the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompileRequest {
    pub manifest_id: ManifestId,
    pub attempt_id: AttemptId,
    /// Recorded, not read from a clock: the same inputs compile to the same
    /// bytes a year later.
    pub resolved_at: Timestamp,
    /// The active-source byte ceiling. Absent means no cap, never zero.
    pub budget: Option<ByteCount>,
    /// The compile's own evidence references (a catalog snapshot, a route
    /// receipt). Recorded sorted and deduplicated.
    pub evidence: Vec<EvidenceId>,
}

/// Why a compile refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompileError {
    /// A candidate speaks to no decision.
    EmptySlot { digest: Digest },
    /// A slot past the identifier byte bound.
    SlotTooLong { digest: Digest },
    /// One digest offered twice. One digest is one candidate; a second
    /// standing or tier for the same bytes is a caller error, not a merge.
    DuplicateCandidate { digest: Digest },
    /// Two or more Ready candidates at the same top tier disagreed on one
    /// slot. Nothing was decided (D5).
    PrecedenceConflict {
        slot: String,
        conflict: PrecedenceConflict,
    },
    /// A `Security`-tier winner did not fit the budget. Refused rather than
    /// cut: a dropped security constraint widens what the run may do.
    SecurityCut {
        digest: Digest,
        bytes: ByteCount,
        remaining: ByteCount,
    },
    /// The active sources' bytes do not fit a `u64`.
    SourceBytesOverflow,
    /// A truth-record bound was exceeded (participations, evidence, counts).
    Bound(TruthRecordError),
    /// The record could not be serialized for digesting.
    Encode(String),
    /// Internal invariant: the record does not carry one row per candidate.
    /// Unreachable by construction; reachable by mutation.
    Incomplete { offered: usize, recorded: usize },
    /// Internal invariant: an emitted tool set exceeds the authority. The
    /// D3 assertion on every path.
    Widened { digest: Digest, tool: String },
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptySlot { digest } => write!(f, "{digest} speaks to no slot"),
            Self::SlotTooLong { digest } => write!(
                f,
                "{digest} names a slot longer than {CONTEXT_ID_MAX_BYTES} bytes"
            ),
            Self::DuplicateCandidate { digest } => {
                write!(f, "{digest} was offered more than once")
            }
            Self::PrecedenceConflict { slot, conflict } => {
                write!(f, "slot {slot:?}: {conflict}")
            }
            Self::SecurityCut {
                digest,
                bytes,
                remaining,
            } => write!(
                f,
                "security-tier {digest} needs {bytes} bytes and {remaining} remain; refusing to \
                 drop a security constraint for room"
            ),
            Self::SourceBytesOverflow => f.write_str("active source bytes overflow u64"),
            Self::Bound(error) => write!(f, "truth-record bound: {error}"),
            Self::Encode(detail) => write!(f, "could not serialize the manifest: {detail}"),
            Self::Incomplete { offered, recorded } => write!(
                f,
                "{offered} candidates offered, {recorded} participation rows recorded"
            ),
            Self::Widened { digest, tool } => write!(
                f,
                "{digest} would be effective for {tool:?}, which the authority never granted"
            ),
        }
    }
}

impl std::error::Error for CompileError {}

impl From<TruthRecordError> for CompileError {
    fn from(error: TruthRecordError) -> Self {
        Self::Bound(error)
    }
}

/// What one compile produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compiled {
    /// The immutable truth record.
    pub manifest: ResolvedManifest,
    /// Per active candidate, the tools it is effective for: its claim
    /// intersected with the authority's grant. Every set is a subset of
    /// [`Authority::tools`].
    pub tools: BTreeMap<Digest, BTreeSet<String>>,
    /// Derived from `manifest` and nothing else — see [`attribution`].
    pub attribution: ContextAttribution,
}

/// The canonical order everything below iterates: highest authority first,
/// then slot, then digest. Never the input order.
fn canonical(a: &Candidate, b: &Candidate) -> std::cmp::Ordering {
    (a.tier, &a.slot, &a.digest).cmp(&(b.tier, &b.slot, &b.digest))
}

/// Compile one resolved manifest from typed inputs.
pub fn compile(
    request: &CompileRequest,
    route: &Route,
    authority: &Authority,
    candidates: &[Candidate],
) -> Result<Compiled, CompileError> {
    // 1. The input is a set of distinct, well-formed candidates. Count it
    //    now; the completeness check at the end is against this number.
    let offered = candidates.len();

    // Validation walks the CANONICAL order, never the arrival order. A
    // refusal is a typed value a caller persists as "why this attempt did not
    // compile", so which offender it names must not depend on the sequence
    // the ports happened to hand the candidates over in: two malformed
    // candidates used to yield a different digest under a permuted input,
    // which made determinism a property of the successful path only. Set
    // membership is unaffected by the reorder — uniqueness is not an
    // ordered question.
    let mut ordered: Vec<&Candidate> = candidates.iter().collect();
    ordered.sort_by(|a, b| canonical(a, b));

    let mut seen: BTreeSet<&Digest> = BTreeSet::new();
    for candidate in &ordered {
        if candidate.slot.is_empty() {
            return Err(CompileError::EmptySlot {
                digest: candidate.digest.clone(),
            });
        }
        if candidate.slot.len() > CONTEXT_ID_MAX_BYTES {
            return Err(CompileError::SlotTooLong {
                digest: candidate.digest.clone(),
            });
        }
        if !seen.insert(&candidate.digest) {
            return Err(CompileError::DuplicateCandidate {
                digest: candidate.digest.clone(),
            });
        }
    }

    // Rows keyed by digest: the map's order IS the record's order, so the
    // canonical participation ordering costs nothing to maintain.
    let mut rows: BTreeMap<&Digest, Participation> = BTreeMap::new();

    // 2. Upstream verdicts are recorded as they arrived; only Ready competes.
    let mut ready: Vec<&Candidate> = Vec::new();
    for candidate in &ordered {
        match candidate.standing.verdict() {
            Some((state, reason)) => {
                rows.insert(
                    &candidate.digest,
                    Participation {
                        digest: candidate.digest.clone(),
                        class: candidate.class,
                        state,
                        reason: Some(reason),
                        detail: None,
                    },
                );
            }
            None => ready.push(candidate),
        }
    }

    // 3. Precedence, one decision per slot. `ready` is in canonical order, so
    //    each group is too, and the winner's index is stable under input
    //    permutation.
    let mut by_slot: BTreeMap<&str, Vec<&Candidate>> = BTreeMap::new();
    for candidate in &ready {
        by_slot
            .entry(candidate.slot.as_str())
            .or_default()
            .push(candidate);
    }
    let mut winners: Vec<&Candidate> = Vec::new();
    for (slot, group) in &by_slot {
        let contributions: Vec<Contribution<&Digest>> = group
            .iter()
            .map(|c| Contribution::new(c.tier, &c.digest))
            .collect();
        let winner = match resolve(&contributions) {
            Ok(Some(index)) => index,
            // A group is built by pushing into it, so it is never empty and
            // the resolver never has nothing to say. If it ever did, the rows
            // it would have produced are missing and step 5 refuses.
            Ok(None) => continue,
            Err(conflict) => {
                return Err(CompileError::PrecedenceConflict {
                    slot: (*slot).to_owned(),
                    conflict,
                });
            }
        };
        for (index, candidate) in group.iter().enumerate() {
            if index == winner {
                winners.push(candidate);
            } else {
                rows.insert(
                    &candidate.digest,
                    Participation::excluded(
                        candidate.digest.clone(),
                        candidate.class,
                        ParticipationReason::PrecedenceLoss,
                    ),
                );
            }
        }
    }

    // 4. Budget, in canonical order. Winners came out grouped by slot; the
    //    admission order is tier-first, so re-sort.
    winners.sort_by(|a, b| canonical(a, b));
    let mut remaining: Option<u64> = request.budget.map(ByteCount::value);
    let mut source_bytes: u64 = 0;
    let mut tools: BTreeMap<Digest, BTreeSet<String>> = BTreeMap::new();
    for candidate in winners {
        let cost = candidate.bytes.value();
        let fits = remaining.is_none_or(|left| cost <= left);
        if !fits {
            if candidate.tier == PrecedenceTier::Security {
                return Err(CompileError::SecurityCut {
                    digest: candidate.digest.clone(),
                    bytes: candidate.bytes,
                    remaining: ByteCount::new(remaining.unwrap_or(u64::MAX)),
                });
            }
            rows.insert(
                &candidate.digest,
                Participation::excluded(
                    candidate.digest.clone(),
                    candidate.class,
                    ParticipationReason::BudgetCut,
                ),
            );
            continue;
        }
        if let Some(left) = remaining.as_mut() {
            *left -= cost;
        }
        source_bytes = source_bytes
            .checked_add(cost)
            .ok_or(CompileError::SourceBytesOverflow)?;
        // INTERSECTION, not union. D3: context narrows authority, never
        // widens it. Swapping this one operation is the whole attack — a
        // candidate would then grant itself whatever it claimed by claiming.
        let effective: BTreeSet<String> = candidate
            .claimed_tools
            .iter()
            .filter(|tool| authority.tools.contains(tool.as_str()))
            .cloned()
            .collect();
        tools.insert(candidate.digest.clone(), effective);
        rows.insert(
            &candidate.digest,
            Participation::active(candidate.digest.clone(), candidate.class),
        );
    }

    // 5. Completeness: candidates in equals rows out. Counted, then compared.
    let recorded = rows.len();
    if recorded != offered {
        return Err(CompileError::Incomplete { offered, recorded });
    }

    // 6. D3, asserted on every path rather than trusted to step 4.
    for (digest, set) in &tools {
        if let Some(tool) = set.iter().find(|t| !authority.tools.contains(t.as_str())) {
            return Err(CompileError::Widened {
                digest: digest.clone(),
                tool: tool.clone(),
            });
        }
    }

    // 7. The record.
    let active = u32::try_from(tools.len()).map_err(|_| TruthRecordError::CountTooLarge)?;
    let mut evidence = request.evidence.clone();
    evidence.sort();
    evidence.dedup();
    let mut manifest = ResolvedManifest {
        id: request.manifest_id.clone(),
        attempt_id: request.attempt_id.clone(),
        manifest_digest: placeholder(),
        route_digest: route.digest.clone(),
        authority_digest: authority.digest.clone(),
        source_count: RecordCount::new(active)?,
        source_bytes: ByteCount::new(source_bytes),
        participations: ParticipationRecords::new(rows.into_values().collect())?,
        evidence_ids: EvidenceRefs::new(evidence)?,
        resolved_at: request.resolved_at.clone(),
    };
    manifest.manifest_digest = manifest_digest(&manifest)?;
    let attribution = attribution(&manifest);
    Ok(Compiled {
        manifest,
        tools,
        attribution,
    })
}

fn placeholder() -> Digest {
    // Pinned by a unit test below: 64 zero hex digits are a legal digest.
    Digest::from_hex(MANIFEST_DIGEST_PLACEHOLDER_HEX)
        .expect("the all-zero placeholder is a legal digest")
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    out
}

/// The manifest's digest under the canonical form (module docs): SHA-256 over
/// the record serialized with `manifest_digest` set to the placeholder.
///
/// Whatever `manifest.manifest_digest` currently holds is ignored, so this is
/// both how the field is filled and how a reader checks it.
pub fn manifest_digest(manifest: &ResolvedManifest) -> Result<Digest, CompileError> {
    let mut preimage = manifest.clone();
    preimage.manifest_digest = placeholder();
    let bytes =
        serde_json::to_vec(&preimage).map_err(|error| CompileError::Encode(error.to_string()))?;
    let raw: [u8; 32] = sha2::Sha256::digest(&bytes).into();
    Digest::from_hex(&hex_lower(&raw)).map_err(|error| CompileError::Encode(error.to_string()))
}

/// Source attribution, re-derived from the resolved manifest and nothing else.
///
/// R12 / CTX-12: the provenance graph never trusts a client-supplied actor
/// string. This function's one input is the record the compiler produced;
/// every field is copied from it or names this build. There is no parameter
/// an input string could arrive through, which is the whole of the control.
pub fn attribution(manifest: &ResolvedManifest) -> ContextAttribution {
    ContextAttribution {
        compiler: AttributionPart::parse(COMPILER)
            .expect("the crate's own name and version are a legal attribution part"),
        route_digest: manifest.route_digest.clone(),
        authority_digest: manifest.authority_digest.clone(),
        derived_from: manifest.id.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_constants_this_module_expects_on_are_legal() {
        // Both `expect`s above are on literals; this is the test that makes
        // them a pinned fact rather than a hope.
        assert_eq!(placeholder().hex(), MANIFEST_DIGEST_PLACEHOLDER_HEX);
        assert_eq!(MANIFEST_DIGEST_PLACEHOLDER_HEX.len(), 64);
        assert!(AttributionPart::parse(COMPILER).is_ok(), "{COMPILER}");
    }

    #[test]
    fn hex_is_lowercase_and_two_digits_per_byte() {
        assert_eq!(hex_lower(&[0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
        assert_eq!(hex_lower(&[]), "");
    }
}
