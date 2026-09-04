//! Explain and Compare, evaluated from recorded truth and nothing else.
//!
//! # The scope is a parameter, not a field
//!
//! [`evaluate`] takes the caller's class scope as an argument supplied by the
//! process, and [`ContextQuery`] has no scope field for a client to set
//! (operator ruling, 2026-09-02). That is CTX-12 a third time: a client cannot
//! assert its own privilege because the wire has nowhere to say it. Task 10
//! made the same choice for attribution, and the reasoning transfers exactly —
//! a field a caller can fill is a field a caller can lie in, and no amount of
//! checking downstream recovers from having asked.
//!
//! # Nothing here reads a file
//!
//! Every answer comes through [`ContextTruthStore`]. That is not a layering
//! preference; it is the property the whole plane exists for. A recomputed
//! manifest answers a question about today and presents it as an answer about
//! the past, which is worse than no answer — so the evaluation path holds no
//! filesystem call, no clock, and no re-derivation from current sources, and
//! `an_explanation_is_the_same_a_week_later` is what holds it to that.
//!
//! # Two scope behaviours, and the difference between them is the point
//!
//! A LISTING (participation, precedence) that withholds rows says how many it
//! withheld. An answer that silently omits reads as complete, and a reader who
//! cannot tell "no private sources" from "private sources you may not see"
//! will draw the first conclusion every time.
//!
//! A PROBE on a caller-supplied digest ([`ExplainSubject::Source`]) does the
//! opposite: a withheld row and an absent row are the SAME answer, byte for
//! byte. Disclosing a count here would answer the question the scope exists to
//! refuse — the caller already holds the digest, so "withheld" tells them the
//! digest is real, is private, and took part in this manifest. Repeat over a
//! digest list and the evaluator is an oracle for private content.
//!
//! Both rules are one principle applied to different questions: never let the
//! shape of a refusal carry the fact it refused.

use gwk_domain::port::StorageError;
use gwk_domain::{
    ContentClass, ContextWireError, Digest, FinalizationSupplement, ManifestId,
    ObservationSupplement, Participation, ParticipationReason, ParticipationState,
    ReleaseSupplement, ResolvedManifest,
};

use crate::stage::ContextStage;
use crate::store::ContextTruthStore;
use crate::wire::{CompareSubject, ContextQuery, ExplainSubject};

/// Whether a caller evaluating under `scope` may see material of `class`.
///
/// Written as four explicit arms rather than two and a wildcard: a third class
/// would be a silent grant under a wildcard, and the whole seam is one
/// decision about who may see what.
pub const fn scope_admits(scope: ContentClass, class: ContentClass) -> bool {
    match (scope, class) {
        (ContentClass::Private, ContentClass::Private) => true,
        (ContentClass::Private, ContentClass::Conformance) => true,
        (ContentClass::Conformance, ContentClass::Conformance) => true,
        (ContentClass::Conformance, ContentClass::Private) => false,
    }
}

/// One participation, as a scope that may see it is told about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExplainedSource {
    pub digest: Digest,
    pub class: ContentClass,
    pub state: ParticipationState,
    pub reason: Option<ParticipationReason>,
    pub detail: Option<String>,
}

impl ExplainedSource {
    fn of(record: &Participation) -> Self {
        Self {
            digest: record.digest.clone(),
            class: record.class,
            state: record.state,
            reason: record.reason,
            detail: record.detail.clone(),
        }
    }
}

/// An Explain answer over a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Explanation {
    pub manifest_id: ManifestId,
    pub subject_is_precedence: bool,
    /// The rows the scope admits, in the manifest's own order.
    pub rows: Vec<ExplainedSource>,
    /// How many rows the scope withheld — a count, never the rows. Zero is a
    /// real answer: it says the listing is complete.
    pub withheld: usize,
}

/// What one stage says about two manifests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StageVerdict {
    /// Both sides carry this stage and agree, over the material the scope
    /// admits.
    Same,
    /// Both sides carry it and disagree.
    Differs,
    /// Neither side has reached this stage.
    NeitherReached,
    /// One side has and the other has not. Which one is named, because
    /// "different" and "not there yet" are different facts about a run.
    OnlyLeft,
    /// As [`Self::OnlyLeft`], the other way round.
    OnlyRight,
    /// No projection records this stage, so no comparison is possible. Not a
    /// synonym for `Same`: an unrecorded stage is unknown, and answering
    /// `Same` about it would be an invention.
    NotRecorded,
}

/// One stage of a comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageDifference {
    pub stage: ContextStage,
    pub verdict: StageVerdict,
    /// How many rows this stage's comparison could not look at. Carried per
    /// stage rather than once for the answer, because a comparison that was
    /// complete at `Released` and blind at `Resolved` is not the same answer
    /// as one blind at both.
    pub withheld: usize,
}

/// A Compare answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comparison {
    pub left: ManifestId,
    pub right: ManifestId,
    /// One entry per stage asked for, in the order asked.
    pub stages: Vec<StageDifference>,
}

/// What [`evaluate`] returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    Explanation(Explanation),
    Comparison(Comparison),
}

/// Why an evaluation produced no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    /// The query is a read this evaluator does not serve. Explain and Compare
    /// are its whole surface; the record and graph reads are other work.
    NotEvaluable,
    /// No manifest under that id.
    NoSuchManifest(ManifestId),
    /// A `Run` comparison subject. The execution DAG has no projection behind
    /// it yet, and comparing two runs by pretending each is its manifest would
    /// answer a different question than the one asked.
    SubjectNotServed,
    /// The store failed. Carried through rather than flattened into "not
    /// found": a read that errored and a record that does not exist are
    /// different facts, and only one of them is an answer.
    Storage(StorageError),
    /// The query is individually well-typed but breaks a rule about how its
    /// parts relate — see [`ContextQuery::validate`]. Carried as the wire error
    /// rather than collapsed to one opaque variant, so the caller is told which
    /// rule it broke.
    ///
    /// [`ContextQuery::validate`]: crate::wire::ContextQuery::validate
    Malformed(ContextWireError),
}

impl From<StorageError> for Refusal {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}

impl From<ContextWireError> for Refusal {
    fn from(error: ContextWireError) -> Self {
        Self::Malformed(error)
    }
}

/// Evaluate one Explain or Compare query under a class scope.
///
/// `scope` comes from the process — see the module docs. `ports` is the only
/// way this function learns anything.
///
/// The relational rules run first, before any port is touched. They were
/// written as [`ContextQuery::validate`] and, until 8B round 4, called from
/// nowhere but that method's own unit test — so a query the wire layer
/// documented as refused was answered here in full.
pub async fn evaluate(
    query: &ContextQuery,
    scope: ContentClass,
    ports: &impl ContextTruthStore,
) -> Result<Answer, Refusal> {
    query.validate()?;
    match query {
        ContextQuery::Explain {
            manifest_id,
            subject,
        } => explain(manifest_id, subject, scope, ports)
            .await
            .map(Answer::Explanation),
        ContextQuery::Compare {
            left,
            right,
            stages,
        } => {
            let left = manifest_of(left)?;
            let right = manifest_of(right)?;
            compare(left, right, stages.get(), scope, ports)
                .await
                .map(Answer::Comparison)
        }
        _ => Err(Refusal::NotEvaluable),
    }
}

fn manifest_of(subject: &CompareSubject) -> Result<&ManifestId, Refusal> {
    match subject {
        CompareSubject::Manifest { manifest_id } => Ok(manifest_id),
        CompareSubject::Run { .. } => Err(Refusal::SubjectNotServed),
    }
}

async fn load(
    id: &ManifestId,
    ports: &impl ContextTruthStore,
) -> Result<ResolvedManifest, Refusal> {
    ports
        .manifest(id)
        .await?
        .ok_or_else(|| Refusal::NoSuchManifest(id.clone()))
}

async fn explain(
    manifest_id: &ManifestId,
    subject: &ExplainSubject,
    scope: ContentClass,
    ports: &impl ContextTruthStore,
) -> Result<Explanation, Refusal> {
    let manifest = load(manifest_id, ports).await?;
    let records = manifest.participations.as_slice();

    match subject {
        // The probe. One row or none, and a row the scope may not see is
        // reported as none — identically, so the answer cannot be read as
        // confirmation. `withheld` is 0 here for the same reason.
        ExplainSubject::Source { digest } => {
            let visible = records
                .iter()
                .find(|record| &record.digest == digest && scope_admits(scope, record.class))
                .map(ExplainedSource::of);
            Ok(Explanation {
                manifest_id: manifest.id,
                subject_is_precedence: false,
                rows: visible.into_iter().collect(),
                withheld: 0,
            })
        }

        // The two listings. Precedence narrows to the rows precedence decided;
        // participation is every row. Both disclose what they withheld.
        ExplainSubject::Precedence | ExplainSubject::Participation => {
            let of_interest: Vec<&Participation> = records
                .iter()
                .filter(|record| match subject {
                    ExplainSubject::Precedence => {
                        record.reason == Some(ParticipationReason::PrecedenceLoss)
                            || record.state == ParticipationState::Active
                    }
                    _ => true,
                })
                .collect();
            let (visible, hidden): (Vec<&Participation>, Vec<&Participation>) = of_interest
                .into_iter()
                .partition(|record| scope_admits(scope, record.class));
            Ok(Explanation {
                manifest_id: manifest.id,
                subject_is_precedence: matches!(subject, ExplainSubject::Precedence),
                rows: visible.into_iter().map(ExplainedSource::of).collect(),
                withheld: hidden.len(),
            })
        }
    }
}

async fn compare(
    left: &ManifestId,
    right: &ManifestId,
    stages: &[ContextStage],
    scope: ContentClass,
    ports: &impl ContextTruthStore,
) -> Result<Comparison, Refusal> {
    let left_manifest = load(left, ports).await?;
    let right_manifest = load(right, ports).await?;

    let mut out = Vec::with_capacity(stages.len());
    for stage in stages {
        out.push(match stage {
            // No declared projection exists. Saying so is the answer.
            ContextStage::Declared => StageDifference {
                stage: *stage,
                verdict: StageVerdict::NotRecorded,
                withheld: 0,
            },
            ContextStage::Resolved => resolved_difference(&left_manifest, &right_manifest, scope),
            ContextStage::Released => {
                let l = ports.release(left).await?;
                let r = ports.release(right).await?;
                pairwise(*stage, l.as_ref(), r.as_ref(), released_same)
            }
            ContextStage::Observed => {
                let l = ports.observations(left).await?;
                let r = ports.observations(right).await?;
                // Absence is an empty vector here rather than `None`, so the
                // "neither reached it" case is empty-and-empty.
                match (l.is_empty(), r.is_empty()) {
                    (true, true) => StageDifference {
                        stage: *stage,
                        verdict: StageVerdict::NeitherReached,
                        withheld: 0,
                    },
                    (false, true) => StageDifference {
                        stage: *stage,
                        verdict: StageVerdict::OnlyLeft,
                        withheld: 0,
                    },
                    (true, false) => StageDifference {
                        stage: *stage,
                        verdict: StageVerdict::OnlyRight,
                        withheld: 0,
                    },
                    (false, false) => StageDifference {
                        stage: *stage,
                        verdict: verdict_of(observed_same(&l, &r)),
                        withheld: 0,
                    },
                }
            }
            ContextStage::Finalized => {
                let l = ports.finalization(left).await?;
                let r = ports.finalization(right).await?;
                pairwise(*stage, l.as_ref(), r.as_ref(), finalized_same)
            }
        });
    }

    Ok(Comparison {
        left: left_manifest.id,
        right: right_manifest.id,
        stages: out,
    })
}

/// `Resolved` compared over the rows the scope admits, and NOT over
/// `manifest_digest`.
///
/// The digest covers every participation including the private ones, so two
/// manifests differing only in private material would come back `Differs` to a
/// conformance-scoped caller — a one-bit channel reporting that private
/// content changed, from a comparison that was supposed to be blind to it.
/// Comparing the admitted rows answers the question the scope actually allows,
/// and `withheld` says how much it could not look at.
fn resolved_difference(
    left: &ResolvedManifest,
    right: &ResolvedManifest,
    scope: ContentClass,
) -> StageDifference {
    let visible = |manifest: &ResolvedManifest| -> Vec<ExplainedSource> {
        manifest
            .participations
            .as_slice()
            .iter()
            .filter(|record| scope_admits(scope, record.class))
            .map(ExplainedSource::of)
            .collect()
    };
    let hidden = |manifest: &ResolvedManifest| -> usize {
        manifest
            .participations
            .as_slice()
            .iter()
            .filter(|record| !scope_admits(scope, record.class))
            .count()
    };
    StageDifference {
        stage: ContextStage::Resolved,
        verdict: verdict_of(visible(left) == visible(right)),
        withheld: hidden(left) + hidden(right),
    }
}

fn pairwise<T>(
    stage: ContextStage,
    left: Option<&T>,
    right: Option<&T>,
    same: fn(&T, &T) -> bool,
) -> StageDifference {
    let verdict = match (left, right) {
        (None, None) => StageVerdict::NeitherReached,
        (Some(_), None) => StageVerdict::OnlyLeft,
        (None, Some(_)) => StageVerdict::OnlyRight,
        (Some(l), Some(r)) => verdict_of(same(l, r)),
    };
    StageDifference {
        stage,
        verdict,
        withheld: 0,
    }
}

const fn verdict_of(same: bool) -> StageVerdict {
    if same {
        StageVerdict::Same
    } else {
        StageVerdict::Differs
    }
}

/// Compared on what was released, never on when or under which id: two
/// identical renders released a minute apart are the same release.
fn released_same(left: &ReleaseSupplement, right: &ReleaseSupplement) -> bool {
    left.rendered_digest == right.rendered_digest
        && left.tool_schema_digest == right.tool_schema_digest
        && left.rendered_bytes == right.rendered_bytes
        && left.tool_schema_count == right.tool_schema_count
}

/// Compared as an ordered sequence of facts. Length first, so a prefix match
/// is not read as agreement.
fn observed_same(left: &[ObservationSupplement], right: &[ObservationSupplement]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(l, r)| {
            l.observation_index == r.observation_index
                && l.fact_digest == r.fact_digest
                && l.truncated == r.truncated
        })
}

/// Compared on the outcome and how well it is attested — `lifecycle_complete`
/// and `assurance` included, because two runs with the same output digest and
/// different assurance did not finish the same way.
fn finalized_same(left: &FinalizationSupplement, right: &FinalizationSupplement) -> bool {
    left.output_digest == right.output_digest
        && left.verification_digest == right.verification_digest
        && left.final_event_root == right.final_event_root
        && left.lifecycle_complete == right.lifecycle_complete
        && left.assurance == right.assurance
}
