//! What the verifier accepts, and what it refuses.
//!
//! The fixtures here are built from `gwk-context`'s public constructors rather
//! than by calling the compiler, and that is forced rather than stylistic: this
//! crate cannot depend on the compiler, so there is no route to its goldens.
//! The gap that leaves — nobody has checked that the two agree — is closed by
//! pinning the digest preimage and its hash as literals below, computed outside
//! this crate. A test asserting my digest equals my digest would pass in every
//! world including the wrong ones.

use std::collections::BTreeSet;

use gwk_context::{
    ContentClass, Digest, EvidenceRefs, FinalizationSupplement, FinalizationSupplementId,
    ManifestId, ObservationIndex, ObservationSupplement, ObservationSupplementId, Participation,
    ParticipationReason, ParticipationRecords, RecordCount, ReleaseSupplement, ReleaseSupplementId,
    ResolvedManifest,
};
use gwk_context_verifier::{
    MANIFEST_DIGEST_PLACEHOLDER_HEX, Package, VerifyError, manifest_digest, verify, verify_manifest,
};
use gwk_domain::{AttemptId, ByteCount, EvidenceId, Timestamp};

/// A digest whose ordering is legible at a glance: `11…` sorts before `22…`.
fn digest(nibble: char) -> Digest {
    let hex: String = std::iter::repeat_n(nibble, 64).collect();
    Digest::from_hex(&hex).expect("64 hex characters is a digest")
}

fn evidence(name: &str) -> EvidenceId {
    EvidenceId::new(name)
}

/// The shape the compiler emits: every offered candidate carries a row, rows
/// ascend by digest, and `source_count` counts only the admitted ones.
fn manifest() -> ResolvedManifest {
    let rows = vec![
        Participation::active(digest('1'), ContentClass::Private),
        Participation::active(digest('2'), ContentClass::Conformance),
        Participation::excluded(
            digest('3'),
            ContentClass::Private,
            ParticipationReason::BudgetCut,
        ),
    ];
    let mut out = ResolvedManifest {
        id: ManifestId::parse("manifest-1").expect("legal id"),
        attempt_id: AttemptId::new("attempt-1"),
        manifest_digest: Digest::from_hex(MANIFEST_DIGEST_PLACEHOLDER_HEX).expect("placeholder"),
        route_digest: digest('a'),
        authority_digest: digest('b'),
        source_count: RecordCount::new(2).expect("in bounds"),
        source_bytes: ByteCount::new(4_096),
        participations: ParticipationRecords::new(rows).expect("valid rows"),
        evidence_ids: EvidenceRefs::new(vec![evidence("ev-1"), evidence("ev-2")])
            .expect("valid refs"),
        resolved_at: Timestamp::new("2026-09-02T00:00:00Z"),
    };
    out.manifest_digest = manifest_digest(&out).expect("digestible");
    out
}

fn known() -> BTreeSet<EvidenceId> {
    [evidence("ev-1"), evidence("ev-2"), evidence("ev-3")]
        .into_iter()
        .collect()
}

fn package(manifest: &ResolvedManifest) -> Package<'_> {
    Package {
        manifest,
        release: None,
        observations: &[],
        finalization: None,
    }
}

#[test]
fn a_well_formed_manifest_verifies() {
    let manifest = manifest();
    assert_eq!(verify_manifest(&manifest), Ok(()));
    assert_eq!(verify(&package(&manifest), &known()), Ok(()));
}

/// The anchor.
///
/// Both literals below were produced OUTSIDE this crate: the JSON is the exact
/// preimage a reader can inspect, and the hash was computed over those bytes by
/// a separate tool. That is what makes this a check rather than an echo — if
/// the digest rule here drifts, there is a fixed point it drifts away from.
///
/// The JSON is pinned as well as the hash, and it earns its place: `source_bytes`
/// appears as the STRING "4096" and `source_count` as the NUMBER 2. `ByteCount`
/// hand-writes its serializer to emit decimal digits as a string, because a u64
/// can exceed what a JSON number safely round-trips. A verifier that assumed
/// both were numbers would hash different bytes and disagree with every manifest
/// ever written — while still looking, in code, exactly correct.
/// Absent `reason` and `detail` are OMITTED, not serialized as null. That was
/// wrong in the first draft of this constant, and only the pin caught it: the
/// digest still agreed with itself either way, because both sides serialize
/// through the same derive. A pin that records only the hash would have been
/// green on a preimage nobody had actually looked at.
const PINNED_PREIMAGE_JSON: &str = r#"{"id":"manifest-1","attempt_id":"attempt-1","manifest_digest":"sha256:0000000000000000000000000000000000000000000000000000000000000000","route_digest":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","authority_digest":"sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","source_count":2,"source_bytes":"4096","participations":[{"digest":"sha256:1111111111111111111111111111111111111111111111111111111111111111","class":"private","state":"active"},{"digest":"sha256:2222222222222222222222222222222222222222222222222222222222222222","class":"conformance","state":"active"},{"digest":"sha256:3333333333333333333333333333333333333333333333333333333333333333","class":"private","state":"excluded","reason":"budget_cut"}],"evidence_ids":["ev-1","ev-2"],"resolved_at":"2026-09-02T00:00:00Z"}"#;

/// Re-pinned at 8B task 11, when a participation began stating its content
/// class: the class is inside the preimage, so every manifest digest changed.
/// That is a breaking change to a value "every manifest ever written is keyed
/// to", and it was allowed on the reachability argument recorded in
/// `schema/steps/be73d920-d1ab71d5.sql` -- no ManifestResolved fact can have
/// been appended, because nothing can reach the one entry point that appends
/// it. If that argument was ever wrong, this constant is where it surfaces.
///
/// SHA-256 of exactly the 832 bytes above, computed by a different language and
/// a different sha256 implementation than the one under test.
const PINNED_DIGEST_HEX: &str = "18c5d000175e1b40074daf64807b7b1b523d32d47114e586435b8207e9b334b0";

#[test]
fn the_digest_preimage_and_its_hash_match_vectors_computed_outside_this_crate() {
    let manifest = manifest();

    let mut preimage = manifest.clone();
    preimage.manifest_digest =
        Digest::from_hex(MANIFEST_DIGEST_PLACEHOLDER_HEX).expect("placeholder");
    let bytes = serde_json::to_vec(&preimage).expect("serializable");
    assert_eq!(
        String::from_utf8(bytes).expect("utf-8"),
        PINNED_PREIMAGE_JSON,
        "the digest preimage changed shape; every manifest ever written is keyed to it"
    );

    assert_eq!(
        manifest_digest(&manifest).expect("digestible"),
        Digest::from_hex(PINNED_DIGEST_HEX).expect("pinned digest is legal")
    );
}

/// The fixture the whole crate exists for: the record is internally consistent
/// — its digest is a correct hash OF ITSELF — and it is still wrong.
///
/// A checker that recomputed only the digest accepts this, because the digest
/// was computed over the reordered rows and matches them perfectly. Ordering is
/// a separate claim, and nothing about the hash can speak to it.
#[test]
fn a_digest_computed_over_reordered_participations_is_still_refused() {
    let mut manifest = manifest();
    let mut rows = manifest.participations.as_slice().to_vec();
    rows.swap(0, 1);
    manifest.participations = ParticipationRecords::new(rows).expect("valid rows");
    // Re-hash so the record agrees with itself. This is the collusion case.
    manifest.manifest_digest = manifest_digest(&manifest).expect("digestible");

    assert_eq!(
        manifest_digest(&manifest).expect("digestible"),
        manifest.manifest_digest,
        "precondition: the digest arm must PASS, or this proves nothing about ordering"
    );
    assert_eq!(
        verify_manifest(&manifest),
        Err(VerifyError::ParticipationOrder { position: 1 })
    );
}

#[test]
fn a_duplicated_participation_is_refused_rather_than_merged() {
    let mut manifest = manifest();
    let rows = vec![
        Participation::active(digest('1'), ContentClass::Private),
        Participation::active(digest('1'), ContentClass::Private),
    ];
    manifest.participations = ParticipationRecords::new(rows).expect("valid rows");
    manifest.source_count = RecordCount::new(2).expect("in bounds");
    manifest.manifest_digest = manifest_digest(&manifest).expect("digestible");

    assert_eq!(
        verify_manifest(&manifest),
        Err(VerifyError::ParticipationDuplicate {
            digest: digest('1')
        })
    );
}

/// A total inconsistent with its parts, agreed on by the record and its hash.
#[test]
fn a_source_count_disagreeing_with_the_admitted_rows_is_refused() {
    let mut manifest = manifest();
    manifest.source_count = RecordCount::new(3).expect("in bounds");
    manifest.manifest_digest = manifest_digest(&manifest).expect("digestible");

    assert_eq!(
        verify_manifest(&manifest),
        Err(VerifyError::SourceCount {
            recorded: 3,
            active: 2
        })
    );
}

#[test]
fn a_tampered_digest_is_refused() {
    let mut manifest = manifest();
    let recorded = digest('c');
    manifest.manifest_digest = recorded.clone();

    match verify_manifest(&manifest) {
        Err(VerifyError::ManifestDigest {
            recorded: got,
            recomputed,
        }) => {
            assert_eq!(got, recorded);
            assert_ne!(recomputed, recorded);
        }
        other => panic!("expected a digest mismatch, got {other:?}"),
    }
}

#[test]
fn a_release_citing_evidence_no_store_holds_is_refused() {
    let manifest = manifest();
    let release = ReleaseSupplement {
        id: ReleaseSupplementId::parse("release-1").expect("legal id"),
        manifest_id: manifest.id.clone(),
        rendered_digest: digest('d'),
        tool_schema_digest: digest('e'),
        rendered_bytes: ByteCount::new(512),
        tool_schema_count: RecordCount::new(1).expect("in bounds"),
        evidence_ids: EvidenceRefs::new(vec![evidence("ev-absent")]).expect("valid refs"),
        released_at: Timestamp::new("2026-09-02T00:01:00Z"),
    };
    let package = Package {
        manifest: &manifest,
        release: Some(&release),
        observations: &[],
        finalization: None,
    };

    assert_eq!(
        verify(&package, &known()),
        Err(VerifyError::EvidenceUnresolved {
            id: evidence("ev-absent")
        })
    );
}

#[test]
fn a_supplement_bound_to_another_manifest_is_refused() {
    let manifest = manifest();
    let release = ReleaseSupplement {
        id: ReleaseSupplementId::parse("release-1").expect("legal id"),
        manifest_id: ManifestId::parse("manifest-other").expect("legal id"),
        rendered_digest: digest('d'),
        tool_schema_digest: digest('e'),
        rendered_bytes: ByteCount::new(512),
        tool_schema_count: RecordCount::new(1).expect("in bounds"),
        evidence_ids: EvidenceRefs::new(vec![evidence("ev-1")]).expect("valid refs"),
        released_at: Timestamp::new("2026-09-02T00:01:00Z"),
    };
    let package = Package {
        manifest: &manifest,
        release: Some(&release),
        observations: &[],
        finalization: None,
    };

    assert_eq!(
        verify(&package, &known()),
        Err(VerifyError::SupplementBinding {
            named: ManifestId::parse("manifest-other").expect("legal id"),
            expected: manifest.id.clone(),
        })
    );
}

fn observation(manifest: &ResolvedManifest, index: u32) -> ObservationSupplement {
    ObservationSupplement {
        id: ObservationSupplementId::parse(&format!("obs-{index}")).expect("legal id"),
        manifest_id: manifest.id.clone(),
        observation_index: ObservationIndex::new(index).expect("nonzero"),
        fact_digest: digest('4'),
        observed_bytes: ByteCount::new(64),
        visible_source_count: RecordCount::new(2).expect("in bounds"),
        truncated: false,
        evidence_ids: EvidenceRefs::new(vec![evidence("ev-1")]).expect("valid refs"),
        observed_at: Timestamp::new("2026-09-02T00:02:00Z"),
    }
}

#[test]
fn observations_must_run_one_through_n_in_order() {
    let manifest = manifest();
    let out_of_order = [observation(&manifest, 2), observation(&manifest, 1)];
    let package = Package {
        manifest: &manifest,
        release: None,
        observations: &out_of_order,
        finalization: None,
    };

    assert_eq!(
        verify(&package, &known()),
        Err(VerifyError::ObservationSequence {
            position: 0,
            index: 2
        })
    );
}

#[test]
fn an_observation_gap_is_refused() {
    let manifest = manifest();
    let gapped = [observation(&manifest, 1), observation(&manifest, 3)];
    let package = Package {
        manifest: &manifest,
        release: None,
        observations: &gapped,
        finalization: None,
    };

    assert_eq!(
        verify(&package, &known()),
        Err(VerifyError::ObservationSequence {
            position: 1,
            index: 3
        })
    );
}

fn finalization(
    manifest: &ResolvedManifest,
    observation_count: u32,
    lifecycle_complete: bool,
) -> FinalizationSupplement {
    FinalizationSupplement {
        id: FinalizationSupplementId::parse("final-1").expect("legal id"),
        manifest_id: manifest.id.clone(),
        output_digest: digest('5'),
        verification_digest: digest('6'),
        approval_count: RecordCount::new(1).expect("in bounds"),
        observation_count: RecordCount::new(observation_count).expect("in bounds"),
        final_event_root: digest('7'),
        lifecycle_complete,
        assurance: gwk_context::Assurance::Trace,
        evidence_ids: EvidenceRefs::new(vec![evidence("ev-1")]).expect("valid refs"),
        finalized_at: Timestamp::new("2026-09-02T00:03:00Z"),
    }
}

#[test]
fn a_finalization_miscounting_its_observations_is_refused() {
    let manifest = manifest();
    let observations = [observation(&manifest, 1)];
    let final_record = finalization(&manifest, 5, false);
    let package = Package {
        manifest: &manifest,
        release: None,
        observations: &observations,
        finalization: Some(&final_record),
    };

    assert_eq!(
        verify(&package, &known()),
        Err(VerifyError::ObservationCount {
            recorded: 5,
            observed: 1
        })
    );
}

#[test]
fn a_finalization_claiming_a_complete_lifecycle_without_a_release_is_refused() {
    let manifest = manifest();
    let final_record = finalization(&manifest, 0, true);
    let package = Package {
        manifest: &manifest,
        release: None,
        observations: &[],
        finalization: Some(&final_record),
    };

    assert_eq!(
        verify(&package, &known()),
        Err(VerifyError::LifecycleIncomplete)
    );
}

#[test]
fn evidence_ids_must_be_sorted_and_unique() {
    let mut unsorted = manifest();
    unsorted.evidence_ids =
        EvidenceRefs::new(vec![evidence("ev-2"), evidence("ev-1")]).expect("valid refs");
    unsorted.manifest_digest = manifest_digest(&unsorted).expect("digestible");
    assert_eq!(
        verify_manifest(&unsorted),
        Err(VerifyError::EvidenceOrder { position: 1 })
    );

    let mut duplicated = manifest();
    duplicated.evidence_ids =
        EvidenceRefs::new(vec![evidence("ev-1"), evidence("ev-1")]).expect("valid refs");
    duplicated.manifest_digest = manifest_digest(&duplicated).expect("digestible");
    assert_eq!(
        verify_manifest(&duplicated),
        Err(VerifyError::EvidenceDuplicate {
            id: evidence("ev-1")
        })
    );
}

// ============================================================
// The supplement-side refusals
// ============================================================
//
// `verify` makes six calls into the two supplement checks: `binds_to` and
// `evidence_is_ordered_and_unique`, once each for a release, an observation and
// a finalization. Five of the six had never refused anything.
//
// The cause was one property shared by every fixture above: each supplement
// cites exactly ONE evidence id. `evidence_is_ordered_and_unique` folds over
// `windows(2)`, and a one-element slice yields no pairs, so all three of its
// call sites ran over nothing and returned Ok. The binding check was unexercised
// for a plainer reason — only the release ever had a fixture naming the wrong
// manifest.
//
// Round 4 measured it by deleting all five lines at once: the crate stayed green
// at 18 tests. Each one below observes exactly one of them.

/// The evidence-order rule needs a supplement citing TWO ids to have any pair to
/// compare. Both ids resolve, so the refusal can only come from their order.
fn two_ids_out_of_order() -> EvidenceRefs {
    EvidenceRefs::new(vec![evidence("ev-2"), evidence("ev-1")]).expect("valid refs")
}

#[test]
fn a_release_citing_its_evidence_out_of_order_is_refused() {
    let manifest = manifest();
    let mut release = ReleaseSupplement {
        id: ReleaseSupplementId::parse("release-1").expect("legal id"),
        manifest_id: manifest.id.clone(),
        rendered_digest: digest('d'),
        tool_schema_digest: digest('e'),
        rendered_bytes: ByteCount::new(512),
        tool_schema_count: RecordCount::new(1).expect("in bounds"),
        evidence_ids: two_ids_out_of_order(),
        released_at: Timestamp::new("2026-09-02T00:01:00Z"),
    };
    let package = Package {
        manifest: &manifest,
        release: Some(&release),
        observations: &[],
        finalization: None,
    };
    assert_eq!(
        verify(&package, &known()),
        Err(VerifyError::EvidenceOrder { position: 1 })
    );

    // The control: the same two ids, sorted, verify. Without it this test also
    // passes in a build that refuses every release citing two of anything.
    release.evidence_ids =
        EvidenceRefs::new(vec![evidence("ev-1"), evidence("ev-2")]).expect("valid refs");
    let package = Package {
        manifest: &manifest,
        release: Some(&release),
        observations: &[],
        finalization: None,
    };
    assert_eq!(verify(&package, &known()), Ok(()));
}

#[test]
fn an_observation_bound_to_another_manifest_is_refused() {
    let manifest = manifest();
    let mut wrong = observation(&manifest, 1);
    wrong.manifest_id = ManifestId::parse("manifest-other").expect("legal id");
    let observations = [wrong];
    let package = Package {
        manifest: &manifest,
        release: None,
        observations: &observations,
        finalization: None,
    };
    assert_eq!(
        verify(&package, &known()),
        Err(VerifyError::SupplementBinding {
            named: ManifestId::parse("manifest-other").expect("legal id"),
            expected: manifest.id.clone(),
        })
    );
}

#[test]
fn an_observation_citing_its_evidence_out_of_order_is_refused() {
    let manifest = manifest();
    let mut unsorted = observation(&manifest, 1);
    unsorted.evidence_ids = two_ids_out_of_order();
    let observations = [unsorted];
    let package = Package {
        manifest: &manifest,
        release: None,
        observations: &observations,
        finalization: None,
    };
    assert_eq!(
        verify(&package, &known()),
        Err(VerifyError::EvidenceOrder { position: 1 })
    );
}

#[test]
fn a_finalization_bound_to_another_manifest_is_refused() {
    let manifest = manifest();
    let mut wrong = finalization(&manifest, 0, false);
    wrong.manifest_id = ManifestId::parse("manifest-other").expect("legal id");
    let package = Package {
        manifest: &manifest,
        release: None,
        observations: &[],
        finalization: Some(&wrong),
    };
    assert_eq!(
        verify(&package, &known()),
        Err(VerifyError::SupplementBinding {
            named: ManifestId::parse("manifest-other").expect("legal id"),
            expected: manifest.id.clone(),
        })
    );
}

#[test]
fn a_finalization_citing_its_evidence_out_of_order_is_refused() {
    let manifest = manifest();
    let mut unsorted = finalization(&manifest, 0, false);
    unsorted.evidence_ids = two_ids_out_of_order();
    let package = Package {
        manifest: &manifest,
        release: None,
        observations: &[],
        finalization: Some(&unsorted),
    };
    assert_eq!(
        verify(&package, &known()),
        Err(VerifyError::EvidenceOrder { position: 1 })
    );
}

/// The duplicate arm, on a supplement rather than on the manifest.
///
/// `evidence_is_ordered_and_unique` has two refusals inside it. Every test above
/// exercises the ordering one, and a rule that had dropped its equality branch
/// would satisfy all of them.
#[test]
fn a_supplement_citing_one_id_twice_is_refused() {
    let manifest = manifest();
    let mut duplicated = observation(&manifest, 1);
    duplicated.evidence_ids =
        EvidenceRefs::new(vec![evidence("ev-1"), evidence("ev-1")]).expect("valid refs");
    let observations = [duplicated];
    let package = Package {
        manifest: &manifest,
        release: None,
        observations: &observations,
        finalization: None,
    };
    assert_eq!(
        verify(&package, &known()),
        Err(VerifyError::EvidenceDuplicate {
            id: evidence("ev-1")
        })
    );
}

/// A whole, well-formed lifecycle: manifest, release, two observations, and a
/// finalization that agrees with all of it.
#[test]
fn a_complete_and_consistent_lifecycle_verifies() {
    let manifest = manifest();
    let release = ReleaseSupplement {
        id: ReleaseSupplementId::parse("release-1").expect("legal id"),
        manifest_id: manifest.id.clone(),
        rendered_digest: digest('d'),
        tool_schema_digest: digest('e'),
        rendered_bytes: ByteCount::new(512),
        tool_schema_count: RecordCount::new(1).expect("in bounds"),
        evidence_ids: EvidenceRefs::new(vec![evidence("ev-1")]).expect("valid refs"),
        released_at: Timestamp::new("2026-09-02T00:01:00Z"),
    };
    let observations = [observation(&manifest, 1), observation(&manifest, 2)];
    let final_record = finalization(&manifest, 2, true);
    let package = Package {
        manifest: &manifest,
        release: Some(&release),
        observations: &observations,
        finalization: Some(&final_record),
    };

    assert_eq!(verify(&package, &known()), Ok(()));
}
