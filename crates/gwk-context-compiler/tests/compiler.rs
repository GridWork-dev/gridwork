//! The compiler's RED suite: the behaviour Task 8 adds, each arm one property
//! the manifest must have and one mutation that would take it away.
//!
//! Every arm that folds counts what it folded over first. A permutation loop
//! that ran zero times and a record that serialized no keys both look like a
//! pass from the far side of an `assert!`.

use std::collections::{BTreeMap, BTreeSet};

use gwk_context::{
    CONTEXT_ID_MAX_BYTES, CONTEXT_PARTICIPATION_MAX_COUNT, ContentClass, Contribution, Digest,
    ManifestId, Participation, ParticipationReason, ParticipationState, PrecedenceTier,
    ResolvedManifest, TruthRecordError,
};
use gwk_context_compiler::{
    Authority, COMPILER, Candidate, CompileError, CompileRequest, Compiled,
    MANIFEST_DIGEST_PLACEHOLDER_HEX, Route, Standing, attribution, compile, manifest_digest,
    resolve,
};
use gwk_domain::{AttemptId, ByteCount, EvidenceId, Timestamp};
use sha2::Digest as _;

// ============================================================
// Helpers
// ============================================================

/// A distinct, legal digest per seed: the seed byte repeated 32 times.
fn digest(seed: u8) -> Digest {
    Digest::from_hex(&format!("{seed:02x}").repeat(32)).expect("64 lowercase hex digits")
}

fn candidate(seed: u8, slot: &str, tier: PrecedenceTier, bytes: u64) -> Candidate {
    // One class for the ordinary fixtures, so a test that cares about the
    // class says so by building its own. `a_candidates_class_rides_into_its_
    // participation_whatever_the_outcome` is that test.
    Candidate {
        digest: digest(seed),
        class: ContentClass::Private,
        slot: slot.to_owned(),
        tier,
        bytes: ByteCount::new(bytes),
        claimed_tools: Vec::new(),
        standing: Standing::Ready,
    }
}

fn request(budget: Option<u64>) -> CompileRequest {
    CompileRequest {
        manifest_id: ManifestId::parse("fixture-manifest").expect("valid id"),
        attempt_id: AttemptId::new("fixture-attempt"),
        resolved_at: Timestamp::new("2026-09-01T00:00:00Z"),
        budget: budget.map(ByteCount::new),
        evidence: vec![
            EvidenceId::new("fixture-evidence-2"),
            EvidenceId::new("fixture-evidence-1"),
            EvidenceId::new("fixture-evidence-2"),
        ],
    }
}

fn route() -> Route {
    Route {
        digest: digest(0xee),
    }
}

fn authority(tools: &[&str]) -> Authority {
    Authority {
        digest: digest(0xaa),
        tools: tools.iter().map(|t| (*t).to_owned()).collect(),
    }
}

/// The mixed fixture every ordering arm runs over: one candidate per outcome
/// the compiler can produce from Ready input, plus one upstream verdict.
///
/// Under a 200-byte budget the canonical admission order is A (Security, 10),
/// F (RouteConfig, 30), B (RequestedSkill, 100), then D (Annotation, 500)
/// which does not fit; C loses `skill-x` to B on tier; E never competes.
fn fixture() -> Vec<Candidate> {
    let mut b = candidate(0xb0, "skill-x", PrecedenceTier::RequestedSkill, 100);
    b.claimed_tools = vec!["Read".into(), "Bash".into()];
    let mut e = candidate(0xe0, "skill-y", PrecedenceTier::AutomaticSkill, 60);
    e.standing = Standing::Quarantined;
    vec![
        candidate(0xa0, "policy", PrecedenceTier::Security, 10),
        b,
        candidate(0xc0, "skill-x", PrecedenceTier::AutomaticSkill, 50),
        candidate(0xd0, "note", PrecedenceTier::Annotation, 500),
        e,
        candidate(0xf0, "skill-z", PrecedenceTier::RouteConfig, 30),
    ]
}

fn row(compiled: &Compiled, seed: u8) -> &Participation {
    let wanted = digest(seed);
    compiled
        .manifest
        .participations
        .as_slice()
        .iter()
        .find(|p| p.digest == wanted)
        .unwrap_or_else(|| panic!("no participation row for {wanted}"))
}

/// Heap's algorithm: every permutation of `items`, visited once each.
fn permutations<T: Clone>(items: &[T]) -> Vec<Vec<T>> {
    fn walk<T: Clone>(k: usize, items: &mut [T], out: &mut Vec<Vec<T>>) {
        if k <= 1 {
            out.push(items.to_vec());
            return;
        }
        walk(k - 1, items, out);
        for i in 0..k - 1 {
            if k.is_multiple_of(2) {
                items.swap(i, k - 1);
            } else {
                items.swap(0, k - 1);
            }
            walk(k - 1, items, out);
        }
    }
    let mut scratch = items.to_vec();
    let mut out = Vec::new();
    walk(scratch.len(), &mut scratch, &mut out);
    out
}

/// Every object key anywhere in a JSON value, including inside arrays.
fn collect_keys(value: &serde_json::Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut stack = vec![value];
    while let Some(node) = stack.pop() {
        match node {
            serde_json::Value::Object(map) => {
                for (k, v) in map {
                    out.insert(k.clone());
                    stack.push(v);
                }
            }
            serde_json::Value::Array(items) => stack.extend(items),
            _ => {}
        }
    }
    out
}

// ============================================================
// Determinism — the discriminating arm
// ============================================================

#[test]
fn identical_inputs_under_candidate_permutation_compile_to_byte_identical_manifests() {
    let orders = permutations(&fixture());
    // 6! orderings. The count first: a permutation generator that yielded one
    // ordering would make every comparison below a comparison with itself.
    assert_eq!(orders.len(), 720);

    // And the count is not enough, which round 4 measured by deleting both of
    // Heap's swaps: the walk still pushes 720 vectors, every one of them the
    // input order untouched, and every comparison below then compares the
    // reference with itself 720 times. The suite stayed green.
    //
    // Distinctness is what the count was standing in for. Keyed on the digest
    // sequence because that is what "a different input order" means to the
    // compiler, and 720 DISTINCT orderings of six items is all of them — so
    // this closes the claim the test's name makes rather than sampling it.
    let distinct: BTreeSet<Vec<String>> = orders
        .iter()
        .map(|order| order.iter().map(|c| c.digest.to_string()).collect())
        .collect();
    assert_eq!(
        distinct.len(),
        720,
        "the generator yielded {} orderings but only {} of them differ",
        orders.len(),
        distinct.len()
    );

    let reference = compile(
        &request(Some(200)),
        &route(),
        &authority(&["Read"]),
        &orders[0],
    )
    .expect("the fixture compiles");
    let reference_bytes = serde_json::to_vec(&reference.manifest).expect("serializes");

    let mut compared = 0usize;
    for order in &orders {
        let compiled = compile(&request(Some(200)), &route(), &authority(&["Read"]), order)
            .expect("every ordering compiles");
        let bytes = serde_json::to_vec(&compiled.manifest).expect("serializes");
        assert_eq!(
            bytes, reference_bytes,
            "manifest bytes moved with input order"
        );
        assert_eq!(
            compiled.manifest.manifest_digest,
            reference.manifest.manifest_digest
        );
        assert_eq!(compiled.tools, reference.tools);
        assert_eq!(compiled.attribution, reference.attribution);
        compared += 1;
    }
    assert_eq!(compared, 720);

    // And the reference is the record the fixture doc promises, so "identical
    // under permutation" is not "identically empty".
    assert_eq!(reference.manifest.source_count.value(), 3);
    assert_eq!(reference.manifest.source_bytes.value(), 140);
    assert_eq!(row(&reference, 0xa0).state, ParticipationState::Active);
    assert_eq!(row(&reference, 0xf0).state, ParticipationState::Active);
    assert_eq!(row(&reference, 0xb0).state, ParticipationState::Active);
    assert_eq!(
        row(&reference, 0xc0).reason,
        Some(ParticipationReason::PrecedenceLoss)
    );
    assert_eq!(
        row(&reference, 0xd0).reason,
        Some(ParticipationReason::BudgetCut)
    );
    assert_eq!(
        row(&reference, 0xe0).reason,
        Some(ParticipationReason::Quarantined)
    );
}

#[test]
fn participations_are_ordered_by_digest_and_evidence_is_sorted_and_deduplicated() {
    // The canonical form the verifier recomputes: stated here as a property of
    // the output, independently of how the compiler happened to build it.
    let compiled = compile(&request(None), &route(), &authority(&[]), &fixture())
        .expect("the fixture compiles");
    let digests: Vec<&Digest> = compiled
        .manifest
        .participations
        .as_slice()
        .iter()
        .map(|p| &p.digest)
        .collect();
    assert_eq!(digests.len(), 6);
    assert!(
        digests.windows(2).all(|w| w[0] < w[1]),
        "participations are not strictly ascending by digest: {digests:?}"
    );

    let evidence: Vec<&str> = compiled
        .manifest
        .evidence_ids
        .as_slice()
        .iter()
        .map(EvidenceId::as_str)
        .collect();
    assert_eq!(evidence, ["fixture-evidence-1", "fixture-evidence-2"]);
}

#[test]
fn the_manifest_digest_is_recomputable_from_the_documented_rule() {
    // What the verifier will do with `gwk-context` types alone: zero the
    // digest field, serialize the STRUCT, hash. If this test needs anything
    // from this crate beyond the placeholder literal, the rule is not
    // independently recomputable and the docs are lying.
    let compiled = compile(
        &request(Some(200)),
        &route(),
        &authority(&["Read"]),
        &fixture(),
    )
    .expect("the fixture compiles");
    let mut preimage: ResolvedManifest = compiled.manifest.clone();
    preimage.manifest_digest =
        Digest::from_hex(MANIFEST_DIGEST_PLACEHOLDER_HEX).expect("the placeholder is legal");
    let bytes = serde_json::to_vec(&preimage).expect("serializes");
    let raw: [u8; 32] = sha2::Sha256::digest(&bytes).into();
    let hex: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    let recomputed = Digest::from_hex(&hex).expect("valid hex");

    assert_eq!(recomputed, compiled.manifest.manifest_digest);
    assert_eq!(
        manifest_digest(&compiled.manifest).expect("digestible"),
        compiled.manifest.manifest_digest
    );
    assert_eq!(MANIFEST_DIGEST_PLACEHOLDER_HEX, "0".repeat(64));
    // And the digest covers the record: change one byte of the input and the
    // digest moves. Without this the three equalities above hold for a digest
    // over the empty string.
    let mut moved = fixture();
    moved[0].bytes = ByteCount::new(11);
    let other =
        compile(&request(Some(200)), &route(), &authority(&["Read"]), &moved).expect("compiles");
    assert_ne!(
        other.manifest.manifest_digest,
        compiled.manifest.manifest_digest
    );
}

// ============================================================
// Precedence — equal authority fails closed
// ============================================================

#[test]
fn an_equal_tier_conflict_fails_closed_with_a_typed_refusal() {
    let mut candidates = fixture();
    // C now matches B's tier: two different digests for `skill-x` at equal
    // authority is a question nobody answered, not a tie to break.
    candidates[2].tier = PrecedenceTier::RequestedSkill;
    let error = compile(&request(None), &route(), &authority(&[]), &candidates)
        .expect_err("an equal-tier disagreement must refuse the whole compile");
    match &error {
        CompileError::PrecedenceConflict { slot, conflict } => {
            assert_eq!(slot, "skill-x");
            assert_eq!(conflict.tier, PrecedenceTier::RequestedSkill);
            assert_eq!(conflict.distinct_values, 2);
        }
        other => panic!("expected a precedence conflict, got {other:?}"),
    }
    assert!(error.to_string().contains("skill-x"), "{error}");

    // The positive control: at different tiers the same two candidates
    // resolve, and the loser says why.
    let compiled = compile(&request(None), &route(), &authority(&[]), &fixture())
        .expect("different tiers resolve");
    assert_eq!(row(&compiled, 0xb0).state, ParticipationState::Active);
    assert_eq!(row(&compiled, 0xc0).state, ParticipationState::Excluded);
    assert_eq!(
        row(&compiled, 0xc0).reason,
        Some(ParticipationReason::PrecedenceLoss)
    );
}

#[test]
fn an_equal_tier_disagreement_fails_closed_rather_than_picking() {
    // The precision golden that moved here with the resolver from
    // `gwk-context`'s hostile-intake corpus, unchanged in substance.
    let contributions = vec![
        Contribution::new(PrecedenceTier::Security, "deny"),
        Contribution::new(PrecedenceTier::Security, "allow"),
        Contribution::new(PrecedenceTier::Annotation, "irrelevant"),
    ];
    let conflict = resolve(&contributions).expect_err("an equal-tier disagreement must fail");
    assert_eq!(conflict.tier, PrecedenceTier::Security);
    assert_eq!(conflict.distinct_values, 2);
    assert_eq!(conflict.reason(), ParticipationReason::PrecedenceLoss);

    // Agreement at the same tier is agreement, not a conflict — the other half,
    // without which "fails closed" could just mean "always fails".
    let agreed = vec![
        Contribution::new(PrecedenceTier::Security, "deny"),
        Contribution::new(PrecedenceTier::Security, "deny"),
        Contribution::new(PrecedenceTier::RunDeclaration, "allow"),
    ];
    assert_eq!(resolve(&agreed).expect("agreement resolves"), Some(0));

    // And nobody speaking resolves to nobody, rather than to a default.
    let empty: Vec<Contribution<&str>> = Vec::new();
    assert_eq!(
        resolve(&empty).expect("an empty set is not a conflict"),
        None
    );
}

// ============================================================
// Authority — context narrows, never widens (D3)
// ============================================================

#[test]
fn claimed_tools_beyond_the_authority_participate_with_the_intersection() {
    // B claims Read and Bash; the authority grants Read and Grep.
    let compiled = compile(
        &request(None),
        &route(),
        &authority(&["Read", "Grep"]),
        &fixture(),
    )
    .expect("compiles");
    assert_eq!(row(&compiled, 0xb0).state, ParticipationState::Active);

    let effective = compiled
        .tools
        .get(&digest(0xb0))
        .expect("an active candidate has a tool set");
    assert_eq!(effective, &BTreeSet::from(["Read".to_owned()]));
    // Both directions, by name, so a union would fail on the first and a
    // pass-through of the grant on the second.
    assert!(
        !effective.contains("Bash"),
        "a claimed but ungranted tool became effective"
    );
    assert!(
        !effective.contains("Grep"),
        "a granted but unclaimed tool became effective"
    );

    // The invariant on every path: nothing in the output exceeds the input.
    // Uncapped, four of the fixture's candidates are active (A, F, B, D).
    let granted = authority(&["Read", "Grep"]).tools;
    let mut checked = 0usize;
    for set in compiled.tools.values() {
        assert!(set.is_subset(&granted), "{set:?} is not within {granted:?}");
        checked += 1;
    }
    assert_eq!(checked, 4, "one tool set per active candidate");
    assert_eq!(compiled.manifest.source_count.value(), 4);
}

// ============================================================
// Budget — deterministic cuts, recorded
// ============================================================

#[test]
fn a_candidate_past_budget_is_cut_in_a_deterministic_order() {
    // 40 bytes admits A (Security, 10) and F (RouteConfig, 30) and nothing
    // after them: B (RequestedSkill, 100) is cut even though it outranks
    // the Annotation, because admission walks the tier order and B does not
    // fit in what A and F left.
    let forward =
        compile(&request(Some(40)), &route(), &authority(&[]), &fixture()).expect("compiles");
    assert_eq!(row(&forward, 0xa0).state, ParticipationState::Active);
    assert_eq!(row(&forward, 0xf0).state, ParticipationState::Active);
    assert_eq!(
        row(&forward, 0xb0).reason,
        Some(ParticipationReason::BudgetCut)
    );
    assert_eq!(
        row(&forward, 0xd0).reason,
        Some(ParticipationReason::BudgetCut)
    );
    assert_eq!(forward.manifest.source_bytes.value(), 40);
    assert_eq!(forward.manifest.source_count.value(), 2);

    // The same cuts from the reversed input — the cut order is the tier
    // order, not the arrival order.
    let mut reversed = fixture();
    reversed.reverse();
    let backward =
        compile(&request(Some(40)), &route(), &authority(&[]), &reversed).expect("compiles");
    assert_eq!(
        serde_json::to_vec(&backward.manifest).expect("serializes"),
        serde_json::to_vec(&forward.manifest).expect("serializes")
    );

    // 130 bytes is the discriminating budget. The canonical walk admits A
    // and F (40) and cuts B, which at 100 no longer fits behind them;
    // admission in arrival order would take B instead and starve whichever
    // of A and F came after it. 40 above cannot tell the two apart, because
    // B does not fit either way.
    for reverse in [false, true] {
        let mut input = fixture();
        if reverse {
            input.reverse();
        }
        let compiled =
            compile(&request(Some(130)), &route(), &authority(&[]), &input).expect("compiles");
        assert_eq!(row(&compiled, 0xa0).state, ParticipationState::Active);
        assert_eq!(row(&compiled, 0xf0).state, ParticipationState::Active);
        assert_eq!(
            row(&compiled, 0xb0).reason,
            Some(ParticipationReason::BudgetCut)
        );
        assert_eq!(compiled.manifest.source_bytes.value(), 40);
        assert_eq!(compiled.manifest.source_count.value(), 2);
    }

    // No budget is no cap, never zero.
    let uncapped =
        compile(&request(None), &route(), &authority(&[]), &fixture()).expect("compiles");
    assert_eq!(row(&uncapped, 0xd0).state, ParticipationState::Active);
    assert_eq!(uncapped.manifest.source_bytes.value(), 640);
}

#[test]
fn a_security_tier_source_that_cannot_fit_the_budget_fails_closed() {
    let error = compile(&request(Some(5)), &route(), &authority(&[]), &fixture())
        .expect_err("a security constraint is never dropped for room");
    match error {
        CompileError::SecurityCut { digest: d, .. } => assert_eq!(d, digest(0xa0)),
        other => panic!("expected a security cut refusal, got {other:?}"),
    }
}

// ============================================================
// The record is complete
// ============================================================

#[test]
fn every_candidate_has_exactly_one_participation_record_including_upstream_verdicts() {
    let standings = [
        (Standing::Ready, ParticipationState::Active, None),
        (
            Standing::NotEligible,
            ParticipationState::Excluded,
            Some(ParticipationReason::NotEligible),
        ),
        (
            Standing::Denied,
            ParticipationState::Excluded,
            Some(ParticipationReason::PermissionDenied),
        ),
        (
            Standing::Quarantined,
            ParticipationState::Unavailable,
            Some(ParticipationReason::Quarantined),
        ),
        (
            Standing::Rejected,
            ParticipationState::Unavailable,
            Some(ParticipationReason::Rejected),
        ),
        (
            Standing::PinDrift,
            ParticipationState::Unavailable,
            Some(ParticipationReason::PinDrift),
        ),
        (
            Standing::Unreadable,
            ParticipationState::Unavailable,
            Some(ParticipationReason::Unavailable),
        ),
    ];
    let candidates: Vec<Candidate> = standings
        .iter()
        .enumerate()
        .map(|(i, (standing, _, _))| {
            let seed = u8::try_from(i + 1).expect("small");
            let mut c = candidate(seed, &format!("slot-{i}"), PrecedenceTier::Annotation, 1);
            c.standing = *standing;
            c
        })
        .collect();
    assert_eq!(candidates.len(), 7);

    let compiled =
        compile(&request(None), &route(), &authority(&[]), &candidates).expect("compiles");
    let rows = compiled.manifest.participations.as_slice();
    assert_eq!(rows.len(), candidates.len(), "candidates in, records out");
    let distinct: BTreeSet<&Digest> = rows.iter().map(|p| &p.digest).collect();
    assert_eq!(distinct.len(), rows.len(), "one row per candidate");

    let mut checked = 0usize;
    for (i, (_, state, reason)) in standings.iter().enumerate() {
        let seed = u8::try_from(i + 1).expect("small");
        let p = row(&compiled, seed);
        assert_eq!(p.state, *state, "standing {:?}", standings[i].0);
        assert_eq!(p.reason, *reason, "standing {:?}", standings[i].0);
        assert_eq!(p.validate(), Ok(()));
        checked += 1;
    }
    assert_eq!(checked, 7);
    // Only the one Ready candidate is a source.
    assert_eq!(compiled.manifest.source_count.value(), 1);
    assert_eq!(compiled.tools.len(), 1);
}

#[test]
fn a_duplicate_digest_is_refused_rather_than_merged() {
    let mut candidates = fixture();
    candidates[3].digest = candidates[0].digest.clone();
    let error = compile(&request(None), &route(), &authority(&[]), &candidates)
        .expect_err("one digest is one candidate");
    assert!(
        matches!(error, CompileError::DuplicateCandidate { digest: ref d } if *d == digest(0xa0)),
        "{error:?}"
    );
}

#[test]
fn bounds_fail_closed_rather_than_truncating() {
    let mut empty_slot = fixture();
    empty_slot[1].slot.clear();
    assert!(matches!(
        compile(&request(None), &route(), &authority(&[]), &empty_slot),
        Err(CompileError::EmptySlot { .. })
    ));

    let mut long_slot = fixture();
    long_slot[1].slot = "s".repeat(CONTEXT_ID_MAX_BYTES + 1);
    assert!(matches!(
        compile(&request(None), &route(), &authority(&[]), &long_slot),
        Err(CompileError::SlotTooLong { .. })
    ));

    let mut huge = fixture();
    huge[0].bytes = ByteCount::new(u64::MAX);
    assert!(matches!(
        compile(&request(None), &route(), &authority(&[]), &huge),
        Err(CompileError::SourceBytesOverflow)
    ));

    let over: Vec<Candidate> = (0..=CONTEXT_PARTICIPATION_MAX_COUNT)
        .map(|i| Candidate {
            digest: Digest::from_hex(&format!("{i:064x}")).expect("valid"),
            class: ContentClass::Private,
            slot: format!("slot-{i}"),
            tier: PrecedenceTier::Annotation,
            bytes: ByteCount::new(1),
            claimed_tools: Vec::new(),
            standing: Standing::Ready,
        })
        .collect();
    assert_eq!(over.len(), CONTEXT_PARTICIPATION_MAX_COUNT + 1);
    assert!(matches!(
        compile(&request(None), &route(), &authority(&[]), &over),
        Err(CompileError::Bound(TruthRecordError::TooManyParticipations))
    ));
}

#[test]
fn the_manifest_binds_each_field_to_the_input_it_came_from() {
    // Every assertion here has an INPUT on one side. The attribution arms
    // below compare the manifest against itself, which is the right shape for
    // "attribution is derived from the record" and no check at all for "the
    // record is derived from the request": under a swap of the two digest
    // initializers both sides of those assertions move together and stay
    // equal, so the manifest can name the authority as the route it compiled
    // against and every arm stays green.
    let request = request(Some(200));
    let compiled =
        compile(&request, &route(), &authority(&["Read"]), &fixture()).expect("compiles");

    assert_ne!(
        digest(0xee),
        digest(0xaa),
        "the two anchors must differ or a swap between them is unobservable"
    );
    assert_eq!(compiled.manifest.route_digest, digest(0xee));
    assert_eq!(compiled.manifest.authority_digest, digest(0xaa));
    assert_eq!(compiled.manifest.resolved_at, request.resolved_at);
    assert_eq!(compiled.manifest.id, request.manifest_id);
    assert_eq!(compiled.manifest.attempt_id, request.attempt_id);
}

#[test]
fn a_refusal_names_the_same_offender_under_either_input_order() {
    // One malformed candidate cannot show this: with a single offender every
    // order names it. Two are needed, and then the refusal is stable only if
    // validation walks the canonical order. A caller persists the typed
    // refusal as "why this attempt did not compile", so an answer that moves
    // with the order the ports handed candidates over in is not an answer.
    let mut first = candidate(0x11, "x", PrecedenceTier::Annotation, 1);
    first.slot.clear();
    let mut second = candidate(0x22, "y", PrecedenceTier::Annotation, 1);
    second.slot.clear();

    let forward = compile(
        &request(None),
        &route(),
        &authority(&[]),
        &[first.clone(), second.clone()],
    )
    .expect_err("two empty slots must refuse");
    let reversed = compile(&request(None), &route(), &authority(&[]), &[second, first])
        .expect_err("two empty slots must refuse");

    assert!(
        matches!(forward, CompileError::EmptySlot { .. }),
        "{forward:?}"
    );
    assert_eq!(
        forward, reversed,
        "the refusal is a function of the candidate SET, not of its order"
    );
}

#[test]
fn the_manifest_id_is_the_only_request_value_attribution_carries() {
    // The carve-out that `no_client_actor_string_reaches_the_attribution`
    // names, pinned to exactly one field. The sentinel rides in every
    // free-text input AND in the manifest id — the one request value
    // attribution legitimately carries — so it may surface in `derived_from`
    // and nowhere else. A second request-derived value reaching attribution
    // moves the count off one.
    const SENTINEL: &str = "SPOOF-ACTOR-7";
    let mut candidates = fixture();
    for c in &mut candidates {
        c.slot = format!("{SENTINEL}-{}", c.slot);
        c.claimed_tools.push(SENTINEL.to_owned());
    }
    let mut spoofed = request(Some(200));
    spoofed.manifest_id = ManifestId::parse(SENTINEL).expect("a legal manifest id");
    spoofed.evidence.push(EvidenceId::new(SENTINEL));

    let compiled = compile(
        &spoofed,
        &route(),
        &authority(&["Read", SENTINEL]),
        &candidates,
    )
    .expect("compiles");

    assert_eq!(compiled.attribution.derived_from, compiled.manifest.id);
    let json = serde_json::to_string(&compiled.attribution).expect("serializes");
    assert_eq!(
        json.matches(SENTINEL).count(),
        1,
        "the sentinel reached attribution somewhere other than derived_from: {json}"
    );
}

// ============================================================
// Attribution — derived from the record, never from an input (R12)
// ============================================================

#[test]
fn no_client_actor_string_reaches_the_attribution() {
    // The sentinel rides in every free-text input that is not a record
    // identity: slot names, claimed and granted tool names, evidence ids.
    // None of those is an actor, and none is where attribution comes from.
    //
    // THE ONE CARVE-OUT, stated because this fixture's silence would else
    // imply a stronger claim than holds. `derived_from` IS a request value —
    // it is `CompileRequest.manifest_id` — so seeding the sentinel there does
    // put it in the attribution. That is the design and not an R12 breach:
    // attribution names the RECORD it was derived from, and a manifest id is
    // a record identity, not a claim about who acted. R12 forbids trusting a
    // client-supplied ACTOR string; it does not forbid naming the record.
    // `the_manifest_id_is_the_only_request_value_attribution_carries` pins
    // the carve-out to that one field, which is why this test's name says
    // client actor rather than input.
    const SENTINEL: &str = "SPOOF-ACTOR-7";
    let mut candidates = fixture();
    for c in &mut candidates {
        c.slot = format!("{SENTINEL}-{}", c.slot);
        c.claimed_tools.push(SENTINEL.to_owned());
    }
    let mut request = request(Some(200));
    request.evidence.push(EvidenceId::new(SENTINEL));
    let compiled = compile(
        &request,
        &route(),
        &authority(&["Read", SENTINEL]),
        &candidates,
    )
    .expect("compiles");

    let attribution_json = serde_json::to_string(&compiled.attribution).expect("serializes");
    assert!(
        !attribution_json.contains(SENTINEL),
        "an input string reached the attribution: {attribution_json}"
    );
    // Each field is the manifest's own, by equality, and the compiler names
    // itself — the three things the record can vouch for and nothing else.
    assert_eq!(compiled.attribution.compiler.as_str(), COMPILER);
    assert!(COMPILER.starts_with("gwk-context-compiler/"));
    assert_eq!(compiled.attribution.derived_from, compiled.manifest.id);
    assert_eq!(
        compiled.attribution.route_digest,
        compiled.manifest.route_digest
    );
    assert_eq!(
        compiled.attribution.authority_digest,
        compiled.manifest.authority_digest
    );
    assert_eq!(attribution(&compiled.manifest), compiled.attribution);

    // The spoof golden from Task 5, over BOTH records this crate emits: no
    // key a client-supplied actor could land in, checked against keys rather
    // than substrings (`author` matches inside `authority_digest`).
    let mut keys = collect_keys(&serde_json::to_value(&compiled.manifest).expect("value"));
    keys.extend(collect_keys(
        &serde_json::to_value(&compiled.attribution).expect("value"),
    ));
    assert!(keys.contains("manifest_digest") && keys.contains("derived_from"));
    let forbidden = [
        "actor",
        "author",
        "principal",
        "identity",
        "user",
        "requested_by",
        "submitted_by",
        "on_behalf_of",
    ];
    assert_eq!(forbidden.len(), 8);
    for key in forbidden {
        assert!(
            !keys.contains(key),
            "`{key}` is a field on an emitted record"
        );
    }

    // And the sentinel-bearing tool is intersected like any other: it was
    // granted, it was claimed, so it is effective — which proves the sentinel
    // was processed, not dropped on the floor before it could reach anything.
    let by_digest: BTreeMap<&Digest, &BTreeSet<String>> = compiled.tools.iter().collect();
    assert!(by_digest[&digest(0xb0)].contains(SENTINEL));
}

#[test]
fn a_candidates_class_rides_into_its_participation_whatever_the_outcome() {
    // The compiler classifies nothing; it carries what it was handed. The four
    // arms are the four ways a row gets built — an upstream verdict, a
    // precedence loss, a budget cut, and an admitted winner — because each
    // constructs its `Participation` at a different site, and a class dropped
    // at one of them would be invisible from the other three.
    //
    // Mixed classes are not enough on their own, and an earlier revision of
    // this test stopped there while naming that exact hazard. `ContentClass`
    // has two variants, so one pass pins at most one direction per site:
    // replacing a site's `candidate.class` with the constant that site's own
    // fixture already carries is invisible, whatever the other candidates are
    // set to. So the whole fixture runs twice with every class flipped, and a
    // constant at any of the four sites disagrees with one of the two passes.
    let mut passes = 0usize;
    for flip in [false, true] {
        // Exhaustive over the two variants with no wildcard arm: a third
        // content class stops this compiling rather than silently halving the
        // coverage this loop exists to provide.
        let pick = |class: ContentClass| match (flip, class) {
            (false, class) => class,
            (true, ContentClass::Conformance) => ContentClass::Private,
            (true, ContentClass::Private) => ContentClass::Conformance,
        };

        let mut upstream = candidate(1, "slot-a", PrecedenceTier::Annotation, 1);
        upstream.class = pick(ContentClass::Conformance);
        upstream.standing = Standing::Rejected;

        let mut loser = candidate(2, "slot-b", PrecedenceTier::Annotation, 1);
        loser.class = pick(ContentClass::Conformance);
        let mut winner = candidate(3, "slot-b", PrecedenceTier::Security, 1);
        winner.class = pick(ContentClass::Private);

        // Wins its own slot, so it survives precedence and reaches the budget
        // step, then costs more than the admitted winner leaves. Annotation
        // tier deliberately: a Security-tier candidate over budget is a hard
        // error rather than a row, which would exercise a different site.
        let mut cut = candidate(4, "slot-c", PrecedenceTier::Annotation, 1_000);
        cut.class = pick(ContentClass::Private);

        let offered = vec![upstream.clone(), loser.clone(), winner.clone(), cut.clone()];
        let compiled = compile(&request(Some(10)), &route(), &authority(&[]), &offered)
            .expect("the fixture compiles");

        let rows = compiled.manifest.participations.as_slice();
        assert_eq!(rows.len(), 4, "every offered candidate is recorded");
        let mut compared = 0usize;
        for candidate in [&upstream, &loser, &winner, &cut] {
            let row = rows
                .iter()
                .find(|row| row.digest == candidate.digest)
                .expect("a row per candidate");
            assert_eq!(
                row.class, candidate.class,
                "{:?} was recorded under another class than it was offered under (flip={flip})",
                candidate.digest
            );
            compared += 1;
        }
        assert_eq!(
            compared, 4,
            "every candidate was compared, not merely found"
        );

        // The arms are the ones named above, so a future refactor that
        // collapses two of them fails here rather than silently narrowing the
        // test. The reasons are what pin WHICH site built each row: a
        // budget-cut row and a precedence-loss row are both `Excluded`, so
        // without them the fourth arm could go unbuilt with the count still
        // reading four.
        let row_of = |digest: &Digest| {
            rows.iter()
                .find(|row| &row.digest == digest)
                .expect("present")
        };
        // Standing::Rejected verdicts to Unavailable, not Excluded (compile.rs
        // Standing::verdict) -- the arm is the upstream-verdict one either way.
        assert_eq!(
            row_of(&upstream.digest).state,
            ParticipationState::Unavailable
        );
        assert_eq!(row_of(&loser.digest).state, ParticipationState::Excluded);
        assert_eq!(
            row_of(&loser.digest).reason,
            Some(ParticipationReason::PrecedenceLoss)
        );
        assert_eq!(row_of(&winner.digest).state, ParticipationState::Active);
        assert_eq!(row_of(&cut.digest).state, ParticipationState::Excluded);
        assert_eq!(
            row_of(&cut.digest).reason,
            Some(ParticipationReason::BudgetCut),
            "the budget-cut site is the one this arm exists for; a \
             precedence-loss row here would leave it unbuilt"
        );

        passes += 1;
    }
    assert_eq!(passes, 2, "both class directions ran");
}

#[test]
fn the_participation_construction_sites_are_still_the_four_this_suite_covers() {
    // `a_candidates_class_rides_into_its_participation_whatever_the_outcome`
    // covers four construction sites in both class directions. Nothing said
    // there are four. A fifth site added later leaves that test green over its
    // own fixture, with no arm naming what stopped being covered — a guard that
    // narrows silently, which is the one direction a per-fixture assertion
    // cannot see for itself.
    //
    // What this pins is the number of construction EXPRESSIONS in the source
    // text, which is a proxy and is stated as one: it would still count a site
    // that had been commented out, and it cannot see a Participation built in
    // another module. Its job is the direction that actually bites here — a
    // site added without coverage — and naming that limit is cheaper than
    // implying it pins more than it does.
    let source = include_str!("../src/compile.rs");
    assert!(
        source.len() > 1_000,
        "the compiler source did not load, so the count below would fold over nothing"
    );

    // Three constructor spellings, because the sites do not agree on one: a
    // struct literal, two `excluded` calls, and one `active` call. Counting a
    // single spelling would have found one site and called it the whole set.
    let sites = source.matches("Participation {").count()
        + source.matches("Participation::active(").count()
        + source.matches("Participation::excluded(").count();
    assert_eq!(
        sites, 4,
        "compile.rs builds a Participation at a number of sites this suite does not \
         cover; add the new site to the class-carry fixture and move this count in \
         the same commit, so the two cannot drift apart quietly"
    );
}
