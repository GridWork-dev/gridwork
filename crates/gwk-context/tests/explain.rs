//! Explain and Compare under a class scope.
//!
//! The store is in-memory and every one of its futures is ready on the first
//! poll, so these poll once against `Waker::noop` rather than pulling an async
//! runtime into a crate that has none. `block_on` panics rather than parks if
//! anything ever returns `Pending`, which is the honest version of that
//! assumption: it fails loudly the day it stops holding instead of hanging.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use gwk_context::explain::{Answer, Explanation, Refusal, StageVerdict, evaluate, scope_admits};
use gwk_context::stage::ContextStage;
use gwk_context::store::ContextTruthStore;
use gwk_context::wire::{CompareStages, CompareSubject, ContextQuery, ExplainSubject};
use gwk_context::{
    ContentClass, Digest, EvidenceRefs, FinalizationSupplement, FinalizationSupplementId,
    ManifestId, ObservationIndex, ObservationSupplement, ObservationSupplementId, Participation,
    ParticipationReason, ParticipationRecords, RecordCount, ReleaseSupplement, ReleaseSupplementId,
    ResolvedManifest,
};
use gwk_domain::port::StorageError;
use gwk_domain::{Assurance, AttemptId, ByteCount, EvidenceId, Timestamp};

// ============================================================
// The executor
// ============================================================

fn block_on<F: Future>(future: F) -> F::Output {
    match pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("the in-memory store yielded; this executor cannot wake it"),
    }
}

// ============================================================
// The store
// ============================================================

#[derive(Default)]
struct Records {
    manifests: BTreeMap<String, ResolvedManifest>,
    releases: BTreeMap<String, ReleaseSupplement>,
    observations: BTreeMap<String, Vec<ObservationSupplement>>,
    finalizations: BTreeMap<String, FinalizationSupplement>,
}

impl ContextTruthStore for Records {
    async fn manifest(&self, id: &ManifestId) -> Result<Option<ResolvedManifest>, StorageError> {
        Ok(self.manifests.get(id.as_str()).cloned())
    }
    async fn release(
        &self,
        manifest: &ManifestId,
    ) -> Result<Option<ReleaseSupplement>, StorageError> {
        Ok(self.releases.get(manifest.as_str()).cloned())
    }
    async fn observations(
        &self,
        manifest: &ManifestId,
    ) -> Result<Vec<ObservationSupplement>, StorageError> {
        Ok(self
            .observations
            .get(manifest.as_str())
            .cloned()
            .unwrap_or_default())
    }
    async fn finalization(
        &self,
        manifest: &ManifestId,
    ) -> Result<Option<FinalizationSupplement>, StorageError> {
        Ok(self.finalizations.get(manifest.as_str()).cloned())
    }
}

// ============================================================
// Fixtures
// ============================================================

fn digest(seed: char) -> Digest {
    Digest::from_hex(&seed.to_string().repeat(64)).expect("64 hex characters")
}

fn id(name: &str) -> ManifestId {
    ManifestId::parse(name).expect("a legal manifest id")
}

fn manifest(name: &str, rows: Vec<Participation>, digest_seed: char) -> ResolvedManifest {
    ResolvedManifest {
        id: id(name),
        attempt_id: AttemptId::new("a-1"),
        manifest_digest: digest(digest_seed),
        route_digest: digest('a'),
        authority_digest: digest('b'),
        source_count: RecordCount::new(rows.len() as u32).expect("bounded"),
        source_bytes: ByteCount::new(4096),
        participations: ParticipationRecords::new(rows).expect("legal rows"),
        evidence_ids: EvidenceRefs::new(vec![EvidenceId::new("ev-1")]).expect("bounded"),
        resolved_at: Timestamp::new("2026-09-02T00:00:00Z"),
    }
}

/// Two public rows and two private ones. Both classes are represented in both
/// participation states, so a filter that keyed on state rather than class
/// would produce a different answer than a correct one.
fn mixed_rows() -> Vec<Participation> {
    vec![
        Participation::active(digest('1'), ContentClass::Conformance),
        Participation::active(digest('2'), ContentClass::Private),
        Participation::excluded(
            digest('3'),
            ContentClass::Conformance,
            ParticipationReason::BudgetCut,
        ),
        Participation::excluded(
            digest('4'),
            ContentClass::Private,
            ParticipationReason::PermissionDenied,
        ),
    ]
}

fn private_count(rows: &[Participation]) -> usize {
    rows.iter()
        .filter(|row| row.class == ContentClass::Private)
        .count()
}

fn store_with(manifests: Vec<ResolvedManifest>) -> Records {
    let mut records = Records::default();
    for manifest in manifests {
        records
            .manifests
            .insert(manifest.id.as_str().to_owned(), manifest);
    }
    records
}

fn explain_query(name: &str, subject: ExplainSubject) -> ContextQuery {
    ContextQuery::Explain {
        manifest_id: id(name),
        subject,
    }
}

fn explanation(answer: Answer) -> Explanation {
    match answer {
        Answer::Explanation(explanation) => explanation,
        other => panic!("expected an explanation, got {other:?}"),
    }
}

// ============================================================
// R22 — the scope boundary
// ============================================================

#[test]
fn a_conformance_scope_sees_no_private_row_and_is_told_how_many_it_did_not_see() {
    let rows = mixed_rows();

    // THE FLOOR, FIRST. Every assertion below is about private rows being
    // absent from an answer, and all of them pass over a fixture with no
    // private rows in it. This is the assertion that makes the rest mean
    // something.
    let private = private_count(&rows);
    assert_eq!(
        private, 2,
        "the fixture carries no private rows to withhold"
    );
    assert_eq!(rows.len() - private, 2, "and none to admit either");

    let store = store_with(vec![manifest("m-1", rows.clone(), 'c')]);
    let query = explain_query("m-1", ExplainSubject::Participation);

    let public = explanation(
        block_on(evaluate(&query, ContentClass::Conformance, &store)).expect("an answer"),
    );
    assert_eq!(public.rows.len(), 2);
    assert_eq!(public.withheld, private, "the count must name what it hid");
    for row in &public.rows {
        assert_eq!(
            row.class,
            ContentClass::Conformance,
            "a private row reached a conformance-scoped answer: {:?}",
            row.digest
        );
    }
    // By digest, not just by class: a filter that returned the right NUMBER of
    // rows with the wrong identities would pass a class-only check.
    let seen: Vec<&Digest> = public.rows.iter().map(|row| &row.digest).collect();
    assert_eq!(seen, vec![&digest('1'), &digest('3')]);

    // THE POSITIVE CONTROL. Without it, an evaluator that returned nothing at
    // all, or refused every query, would satisfy every assertion above.
    let privileged =
        explanation(block_on(evaluate(&query, ContentClass::Private, &store)).expect("an answer"));
    assert_eq!(privileged.rows.len(), rows.len());
    assert_eq!(privileged.withheld, 0);
    assert!(
        privileged
            .rows
            .iter()
            .any(|row| row.class == ContentClass::Private),
        "the privileged scope saw no private row either, so the filter is not what removed them"
    );
}

#[test]
fn a_source_probe_cannot_tell_a_withheld_row_from_an_absent_one() {
    // The oracle the module docs describe. A caller who already holds a digest
    // learns nothing by asking: withheld and absent must be the same answer.
    let store = store_with(vec![manifest("m-1", mixed_rows(), 'c')]);

    let withheld = explanation(
        block_on(evaluate(
            &explain_query(
                "m-1",
                ExplainSubject::Source {
                    digest: digest('2'),
                },
            ),
            ContentClass::Conformance,
            &store,
        ))
        .expect("an answer"),
    );
    let absent = explanation(
        block_on(evaluate(
            &explain_query(
                "m-1",
                ExplainSubject::Source {
                    digest: digest('9'),
                },
            ),
            ContentClass::Conformance,
            &store,
        ))
        .expect("an answer"),
    );
    assert_eq!(
        withheld, absent,
        "a private source is distinguishable from one that was never offered"
    );
    assert_eq!(withheld.withheld, 0, "the probe disclosed a count");
    assert!(withheld.rows.is_empty());

    // The control, again on the same digest: the row does exist, and a scope
    // entitled to it gets it. Otherwise the two answers above could match
    // because `Source` is broken for every input.
    let seen = explanation(
        block_on(evaluate(
            &explain_query(
                "m-1",
                ExplainSubject::Source {
                    digest: digest('2'),
                },
            ),
            ContentClass::Private,
            &store,
        ))
        .expect("an answer"),
    );
    assert_eq!(seen.rows.len(), 1);
    assert_eq!(seen.rows[0].digest, digest('2'));
    assert_eq!(seen.rows[0].class, ContentClass::Private);
}

#[test]
fn resolved_is_compared_over_admitted_rows_and_not_over_the_manifest_digest() {
    // Two manifests with IDENTICAL public rows, DIFFERENT private rows, and
    // therefore different manifest digests. Comparing digests would report
    // `Differs` to a conformance-scoped caller — a one-bit channel saying
    // private content changed, out of a comparison built to be blind to it.
    let shared = vec![
        Participation::active(digest('1'), ContentClass::Conformance),
        Participation::excluded(
            digest('3'),
            ContentClass::Conformance,
            ParticipationReason::BudgetCut,
        ),
    ];
    let mut left_rows = shared.clone();
    left_rows.push(Participation::active(digest('2'), ContentClass::Private));
    let mut right_rows = shared.clone();
    right_rows.push(Participation::active(digest('4'), ContentClass::Private));
    // Deliberately asymmetric: two private rows on the right against one on
    // the left. With one per side, the sum of both sides and a body that
    // doubled either side alone are the same number, so the assertion below
    // could not tell a two-sided count from a one-sided one.
    right_rows.push(Participation::excluded(
        digest('5'),
        ContentClass::Private,
        ParticipationReason::PermissionDenied,
    ));

    let left = manifest("m-left", left_rows, 'c');
    let right = manifest("m-right", right_rows, 'd');
    assert_ne!(
        left.manifest_digest, right.manifest_digest,
        "the fixture's two manifests are digest-identical, so this proves nothing"
    );

    let store = store_with(vec![left, right]);
    let query = ContextQuery::Compare {
        left: CompareSubject::Manifest {
            manifest_id: id("m-left"),
        },
        right: CompareSubject::Manifest {
            manifest_id: id("m-right"),
        },
        stages: CompareStages::new(vec![ContextStage::Resolved]).expect("one stage"),
    };

    let public = match block_on(evaluate(&query, ContentClass::Conformance, &store)) {
        Ok(Answer::Comparison(comparison)) => comparison,
        other => panic!("expected a comparison, got {other:?}"),
    };
    assert_eq!(public.stages.len(), 1);
    assert_eq!(
        public.stages[0].verdict,
        StageVerdict::Same,
        "the private difference leaked into a conformance-scoped comparison"
    );
    assert_eq!(
        public.stages[0].withheld, 3,
        "one withheld row on the left and two on the right, and the answer must say so"
    );

    // The control: the difference is real, and a scope entitled to see it does.
    let privileged = match block_on(evaluate(&query, ContentClass::Private, &store)) {
        Ok(Answer::Comparison(comparison)) => comparison,
        other => panic!("expected a comparison, got {other:?}"),
    };
    assert_eq!(privileged.stages[0].verdict, StageVerdict::Differs);
    assert_eq!(privileged.stages[0].withheld, 0);
}

// ============================================================
// Compare, per stage
// ============================================================

#[test]
fn each_stage_answers_about_itself_and_an_unrecorded_one_says_so() {
    let mut store = store_with(vec![
        manifest("m-left", mixed_rows(), 'c'),
        manifest("m-right", mixed_rows(), 'c'),
    ]);
    // Left is released, right is not. Neither is observed. Both are finalized,
    // differently — so the four served stages take four different verdicts in
    // one answer, and a stage that copied its neighbour's result would show.
    store.releases.insert(
        "m-left".to_owned(),
        ReleaseSupplement {
            id: ReleaseSupplementId::parse("r-1").expect("legal"),
            manifest_id: id("m-left"),
            rendered_digest: digest('5'),
            tool_schema_digest: digest('6'),
            rendered_bytes: ByteCount::new(1),
            tool_schema_count: RecordCount::new(1).expect("bounded"),
            evidence_ids: EvidenceRefs::new(Vec::new()).expect("bounded"),
            released_at: Timestamp::new("2026-09-02T00:00:00Z"),
        },
    );
    // Trace against Deterministic: two runs that reached the same output under
    // different assurance did not finish the same way, and Finalized says so.
    for (name, assurance) in [
        ("m-left", Assurance::Trace),
        ("m-right", Assurance::Deterministic),
    ] {
        store.finalizations.insert(
            name.to_owned(),
            FinalizationSupplement {
                id: FinalizationSupplementId::parse(&format!("f-{name}")).expect("legal"),
                manifest_id: id(name),
                output_digest: digest('7'),
                verification_digest: digest('8'),
                approval_count: RecordCount::new(1).expect("bounded"),
                observation_count: RecordCount::new(0).expect("bounded"),
                final_event_root: digest('9'),
                lifecycle_complete: true,
                assurance,
                evidence_ids: EvidenceRefs::new(Vec::new()).expect("bounded"),
                finalized_at: Timestamp::new("2026-09-02T00:00:00Z"),
            },
        );
    }

    let query = ContextQuery::Compare {
        left: CompareSubject::Manifest {
            manifest_id: id("m-left"),
        },
        right: CompareSubject::Manifest {
            manifest_id: id("m-right"),
        },
        stages: CompareStages::new(vec![
            ContextStage::Declared,
            ContextStage::Resolved,
            ContextStage::Released,
            ContextStage::Observed,
            ContextStage::Finalized,
        ])
        .expect("five stages"),
    };

    let comparison = match block_on(evaluate(&query, ContentClass::Private, &store)) {
        Ok(Answer::Comparison(comparison)) => comparison,
        other => panic!("expected a comparison, got {other:?}"),
    };

    // As a SET of (stage, verdict) pairs in the order asked. A count of five
    // would hold while two stages swapped answers.
    let verdicts: Vec<(ContextStage, StageVerdict)> = comparison
        .stages
        .iter()
        .map(|difference| (difference.stage, difference.verdict.clone()))
        .collect();
    assert_eq!(
        verdicts,
        vec![
            (ContextStage::Declared, StageVerdict::NotRecorded),
            (ContextStage::Resolved, StageVerdict::Same),
            (ContextStage::Released, StageVerdict::OnlyLeft),
            (ContextStage::Observed, StageVerdict::NeitherReached),
            (ContextStage::Finalized, StageVerdict::Differs),
        ]
    );
}

#[test]
fn a_run_subject_is_refused_rather_than_answered_as_its_manifest() {
    let store = store_with(vec![manifest("m-1", mixed_rows(), 'c')]);
    let query = ContextQuery::Compare {
        left: CompareSubject::Manifest {
            manifest_id: id("m-1"),
        },
        right: CompareSubject::Run {
            run_id: gwk_context::ContextRunId::parse("run-1").expect("legal"),
        },
        stages: CompareStages::new(vec![ContextStage::Resolved]).expect("one stage"),
    };
    assert_eq!(
        block_on(evaluate(&query, ContentClass::Private, &store)),
        Err(Refusal::SubjectNotServed)
    );
}

#[test]
fn a_query_this_evaluator_does_not_serve_is_refused_by_name() {
    let store = store_with(vec![manifest("m-1", mixed_rows(), 'c')]);
    let query = ContextQuery::Source {
        manifest_id: id("m-1"),
        digest: digest('1'),
    };
    assert_eq!(
        block_on(evaluate(&query, ContentClass::Private, &store)),
        Err(Refusal::NotEvaluable)
    );

    let missing = explain_query("m-nope", ExplainSubject::Participation);
    assert_eq!(
        block_on(evaluate(&missing, ContentClass::Private, &store)),
        Err(Refusal::NoSuchManifest(id("m-nope")))
    );
}

// ============================================================
// Reproducibility
// ============================================================

#[test]
fn an_explanation_is_the_same_a_week_later() {
    // "A week later" is not a clock: it is the claim that the answer is a
    // function of the recorded records and nothing else. Two things stand in
    // for the week — the same store answering twice, which catches an
    // evaluator that mutates or caches, and a second store built independently
    // from equal records, which catches one that keyed on store identity.
    let store = store_with(vec![manifest("m-1", mixed_rows(), 'c')]);
    let query = explain_query("m-1", ExplainSubject::Participation);

    let first = block_on(evaluate(&query, ContentClass::Conformance, &store)).expect("an answer");
    let again = block_on(evaluate(&query, ContentClass::Conformance, &store)).expect("an answer");
    assert_eq!(first, again);

    let rebuilt = store_with(vec![manifest("m-1", mixed_rows(), 'c')]);
    let later = block_on(evaluate(&query, ContentClass::Conformance, &rebuilt)).expect("an answer");
    assert_eq!(first, later);

    // And it is not vacuously equal: the answer has content.
    let explanation = explanation(first);
    assert!(!explanation.rows.is_empty());
    assert_eq!(explanation.withheld, 2);
}

#[test]
fn the_evaluation_path_names_no_file_no_clock_and_no_recompilation() {
    // The structural half of the claim above. A behavioural test cannot see a
    // filesystem read that happens to return the same bytes twice; this can.
    //
    // Source-level, and it counts what it inspected first, because a scan over
    // an empty string agrees with every rule anyone writes.
    let source = include_str!("../src/explain.rs");
    let code: Vec<&str> = source
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with("//"))
        .collect();
    assert!(
        code.len() > 100,
        "read {} lines of evaluator body; the scan found nothing to check",
        code.len()
    );

    for forbidden in [
        "std::fs",
        "File::",
        "read_to_string",
        "SystemTime",
        "Instant",
        "now()",
        "std::env",
    ] {
        let hits: Vec<&&str> = code
            .iter()
            .filter(|line| line.contains(forbidden))
            .collect();
        assert!(
            hits.is_empty(),
            "the evaluation path reaches for {forbidden}: {hits:?}"
        );
    }
}

// ============================================================
// The predicate itself
// ============================================================

#[test]
fn the_scope_predicate_is_exhaustive_and_asymmetric() {
    // Written out rather than looped, because the asymmetry IS the rule: three
    // of the four pairs admit and exactly one refuses.
    assert!(scope_admits(ContentClass::Private, ContentClass::Private));
    assert!(scope_admits(
        ContentClass::Private,
        ContentClass::Conformance
    ));
    assert!(scope_admits(
        ContentClass::Conformance,
        ContentClass::Conformance
    ));
    assert!(!scope_admits(
        ContentClass::Conformance,
        ContentClass::Private
    ));

    // And the count, so a third class cannot arrive without this test noticing
    // that the four cases above no longer cover the space.
    assert_eq!(ContentClass::ALL.len(), 2);
}

// ============================================================
// The precedence listing
// ============================================================

#[test]
fn the_precedence_subject_narrows_to_what_precedence_decided_and_says_which_it_is() {
    // Participation lists every row. Precedence lists only the rows precedence
    // itself decided: the sources it admitted, plus the ones it turned down. A
    // row excluded for a reason precedence never weighed — a budget cut — is
    // the discriminator, because it belongs to one listing and not the other.
    // Without such a row in the fixture the two subjects return the same set,
    // and a filter that ignored the subject entirely would be invisible.
    let rows = vec![
        Participation::active(digest('1'), ContentClass::Conformance),
        Participation::excluded(
            digest('2'),
            ContentClass::Conformance,
            ParticipationReason::PrecedenceLoss,
        ),
        Participation::excluded(
            digest('3'),
            ContentClass::Conformance,
            ParticipationReason::BudgetCut,
        ),
    ];
    let store = store_with(vec![manifest("m", rows, 'c')]);
    let ask = |subject| {
        explanation(
            block_on(evaluate(
                &explain_query("m", subject),
                ContentClass::Private,
                &store,
            ))
            .expect("the fixture explains"),
        )
    };

    let precedence = ask(ExplainSubject::Precedence);
    let participation = ask(ExplainSubject::Participation);

    // The label, which a consumer branches on to title the answer. Pinned in
    // both directions: a constant here mislabels every answer without moving a
    // single row, so one direction alone cannot see it.
    assert!(
        precedence.subject_is_precedence,
        "a precedence answer that does not say so is a participation listing under another name"
    );
    assert!(
        !participation.subject_is_precedence,
        "a participation listing must not claim precedence decided it"
    );

    // The narrowing.
    assert_eq!(
        participation.rows.len(),
        3,
        "participation is every row, so the fixture must reach the fold carrying all three"
    );
    assert_eq!(
        precedence.rows.len(),
        2,
        "precedence is the admitted rows plus the precedence losses, and nothing else"
    );
    let budget_cut = digest('3');
    assert!(
        participation
            .rows
            .iter()
            .any(|row| row.digest == budget_cut),
        "the budget-cut row is the discriminator, so participation must carry it"
    );
    assert!(
        !precedence.rows.iter().any(|row| row.digest == budget_cut),
        "a budget cut is not a precedence decision, so precedence must not list it"
    );
}

// ============================================================
// Every field each stage comparator reads
// ============================================================

fn release_for(name: &str) -> ReleaseSupplement {
    ReleaseSupplement {
        id: ReleaseSupplementId::parse(&format!("r-{name}")).expect("legal"),
        manifest_id: id(name),
        rendered_digest: digest('5'),
        tool_schema_digest: digest('6'),
        rendered_bytes: ByteCount::new(1),
        tool_schema_count: RecordCount::new(1).expect("bounded"),
        evidence_ids: EvidenceRefs::new(Vec::new()).expect("bounded"),
        released_at: Timestamp::new("2026-09-02T00:00:00Z"),
    }
}

fn observation_for(name: &str, index: u32) -> ObservationSupplement {
    ObservationSupplement {
        id: ObservationSupplementId::parse(&format!("o-{name}-{index}")).expect("legal"),
        manifest_id: id(name),
        observation_index: ObservationIndex::new(index).expect("bounded"),
        fact_digest: digest('7'),
        observed_bytes: ByteCount::new(2),
        visible_source_count: RecordCount::new(1).expect("bounded"),
        truncated: false,
        evidence_ids: EvidenceRefs::new(Vec::new()).expect("bounded"),
        observed_at: Timestamp::new("2026-09-02T00:00:00Z"),
    }
}

fn finalization_for(name: &str) -> FinalizationSupplement {
    FinalizationSupplement {
        id: FinalizationSupplementId::parse(&format!("f-{name}")).expect("legal"),
        manifest_id: id(name),
        output_digest: digest('8'),
        verification_digest: digest('9'),
        approval_count: RecordCount::new(1).expect("bounded"),
        observation_count: RecordCount::new(2).expect("bounded"),
        final_event_root: digest('a'),
        lifecycle_complete: true,
        assurance: Assurance::Trace,
        evidence_ids: EvidenceRefs::new(Vec::new()).expect("bounded"),
        finalized_at: Timestamp::new("2026-09-02T00:00:00Z"),
    }
}

/// Two manifests identical at every stage until `tweak` changes one thing on
/// the right, compared at one stage under a scope that admits everything.
fn verdict_after(stage: ContextStage, tweak: impl FnOnce(&mut Records)) -> StageVerdict {
    let mut store = store_with(vec![
        manifest("m-left", mixed_rows(), 'c'),
        manifest("m-right", mixed_rows(), 'c'),
    ]);
    for name in ["m-left", "m-right"] {
        store.releases.insert(name.to_owned(), release_for(name));
        store.observations.insert(
            name.to_owned(),
            vec![observation_for(name, 1), observation_for(name, 2)],
        );
        store
            .finalizations
            .insert(name.to_owned(), finalization_for(name));
    }
    tweak(&mut store);

    let query = ContextQuery::Compare {
        left: CompareSubject::Manifest {
            manifest_id: id("m-left"),
        },
        right: CompareSubject::Manifest {
            manifest_id: id("m-right"),
        },
        stages: CompareStages::new(vec![stage]).expect("one stage"),
    };
    let comparison = match block_on(evaluate(&query, ContentClass::Private, &store)) {
        Ok(Answer::Comparison(comparison)) => comparison,
        other => panic!("expected a comparison, got {other:?}"),
    };
    assert_eq!(
        comparison.stages.len(),
        1,
        "one stage asked for, one stage answered"
    );
    comparison.stages[0].verdict.clone()
}

fn right_release(store: &mut Records) -> &mut ReleaseSupplement {
    store.releases.get_mut("m-right").expect("seeded above")
}

fn right_observations(store: &mut Records) -> &mut Vec<ObservationSupplement> {
    store.observations.get_mut("m-right").expect("seeded above")
}

fn right_finalization(store: &mut Records) -> &mut FinalizationSupplement {
    store
        .finalizations
        .get_mut("m-right")
        .expect("seeded above")
}

#[test]
fn every_field_a_stage_comparator_reads_moves_that_stage_verdict() {
    // The per-stage test above builds its fixture so the four served stages
    // take four DIFFERENT verdicts in one answer, which is what makes a stage
    // copying its neighbour's result visible. The price of that design is that
    // two comparators are never CALLED: only the left side is released and
    // neither side is observed, so `pairwise` answers from its presence arms
    // (`OnlyLeft`, `NeitherReached`) and `released_same` / `observed_same` do
    // not run. Seven field comparisons had therefore never executed, including
    // the length rule whose own comment says it exists so that a prefix match
    // is not read as agreement.
    //
    // Here both sides are present and identical at every stage, so each
    // comparator is reached, and every arm moves exactly one field. A
    // comparator that stopped reading a field disagrees with exactly one arm,
    // rather than passing over a fixture in which nothing it dropped ever
    // differed.
    //
    // Deliberately absent: `observed_bytes` and `visible_source_count` on an
    // observation, and `approval_count` / `observation_count` on a
    // finalization. Those are carried but not compared, and an arm for them
    // would assert a semantic this slice did not choose.
    let mut arms = 0usize;

    macro_rules! differs {
        ($stage:expr, $tweak:expr, $why:literal) => {{
            assert_eq!(verdict_after($stage, $tweak), StageVerdict::Differs, $why);
            arms += 1;
        }};
    }

    // The control. Without it every arm below could pass because the
    // comparison says `Differs` to everything.
    for stage in [
        ContextStage::Released,
        ContextStage::Observed,
        ContextStage::Finalized,
    ] {
        assert_eq!(
            verdict_after(stage, |_| {}),
            StageVerdict::Same,
            "two identical sides must compare Same, or every arm below passes for the wrong reason"
        );
        arms += 1;
    }

    differs!(
        ContextStage::Released,
        |s: &mut Records| right_release(s).rendered_digest = digest('e'),
        "released: rendered_digest"
    );
    differs!(
        ContextStage::Released,
        |s: &mut Records| right_release(s).tool_schema_digest = digest('e'),
        "released: tool_schema_digest"
    );
    differs!(
        ContextStage::Released,
        |s: &mut Records| right_release(s).rendered_bytes = ByteCount::new(99),
        "released: rendered_bytes"
    );
    differs!(
        ContextStage::Released,
        |s: &mut Records| right_release(s).tool_schema_count =
            RecordCount::new(9).expect("bounded"),
        "released: tool_schema_count"
    );

    differs!(
        ContextStage::Observed,
        |s: &mut Records| {
            right_observations(s).pop();
        },
        "observed: length, so a prefix match is not read as agreement"
    );
    differs!(
        ContextStage::Observed,
        |s: &mut Records| right_observations(s)[1].observation_index =
            ObservationIndex::new(9).expect("bounded"),
        "observed: observation_index"
    );
    differs!(
        ContextStage::Observed,
        |s: &mut Records| right_observations(s)[0].fact_digest = digest('e'),
        "observed: fact_digest"
    );
    differs!(
        ContextStage::Observed,
        |s: &mut Records| right_observations(s)[0].truncated = true,
        "observed: truncated"
    );

    differs!(
        ContextStage::Finalized,
        |s: &mut Records| right_finalization(s).output_digest = digest('e'),
        "finalized: output_digest"
    );
    differs!(
        ContextStage::Finalized,
        |s: &mut Records| right_finalization(s).verification_digest = digest('e'),
        "finalized: verification_digest"
    );
    differs!(
        ContextStage::Finalized,
        |s: &mut Records| right_finalization(s).final_event_root = digest('e'),
        "finalized: final_event_root"
    );
    differs!(
        ContextStage::Finalized,
        |s: &mut Records| right_finalization(s).lifecycle_complete = false,
        "finalized: lifecycle_complete"
    );
    differs!(
        ContextStage::Finalized,
        |s: &mut Records| right_finalization(s).assurance = Assurance::Deterministic,
        "finalized: assurance"
    );

    assert_eq!(
        arms, 16,
        "three controls and thirteen field arms; a field dropped from this list \
         is a field nothing pins, and the count is what says so"
    );
}
