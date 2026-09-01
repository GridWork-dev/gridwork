//! Hostile intake: what this crate must refuse, proved twice over.
//!
//! R25 asks for a mixed corpus and this file is both halves of it, kept
//! deliberately distinct because they fail differently.
//!
//! **Precision goldens** are hand-authored and reviewed. Each one is a specific
//! shape somebody argued about, and it asserts an exact refusal — the point is
//! that a human decided this input is wrong and said why.
//!
//! **Generated batteries** are programmatic and exist for volume. They sweep an
//! axis (every bound at its edge and one past it, every bundle path shape) so
//! coverage does not depend on somebody remembering the twelfth case. They
//! assert a class of refusal, not a specific message.
//!
//! Both counts are asserted non-zero before either is believed. A battery that
//! generated nothing and a corpus that refused everything look identical from
//! the far side of a `for` loop.
//!
//! ## What is deliberately not here
//!
//! Engine-specific prompt-injection corpora. Those belong to the phase that can
//! see every adapter at once; a corpus written against one adapter's rendering
//! teaches the parser to refuse that adapter's shape and nothing else.

use std::collections::BTreeSet;

use gwk_context::skill::{
    BundleEntry, BundleRefusal, RawEntryKind, SKILL_ALLOWED_TOOL_MAX_BYTES,
    SKILL_ALLOWED_TOOLS_MAX_COUNT, SKILL_BUNDLE_MAX_ENTRIES, SKILL_BUNDLE_PATH_MAX_BYTES,
    SKILL_COMPATIBILITY_MAX_BYTES, SKILL_DESCRIPTION_MAX_BYTES, SKILL_FRONTMATTER_MAX_BYTES,
    SKILL_INPUT_MAX_BYTES, SKILL_LICENSE_MAX_BYTES, SKILL_METADATA_KEY_MAX_BYTES,
    SKILL_METADATA_MAX_ENTRIES, SKILL_METADATA_VALUE_MAX_BYTES, SKILL_NAME_MAX_BYTES,
    SKILL_OPAQUE_FIELD_MAX_COUNT, SkillError, SkillManifest,
};
use gwk_context::{
    CONTEXT_EVIDENCE_MAX_COUNT, Digest, EvidenceRefs, Participation, ParticipationReason,
    ResolvedManifest, TruthRecordError,
};
use gwk_domain::EvidenceId;

// ============================================================
// Helpers
// ============================================================

fn manifest_with(front: &str) -> Result<SkillManifest, SkillError> {
    SkillManifest::parse(&format!("---\n{front}\n---\n\nbody\n"))
}

fn digest() -> Digest {
    Digest::from_hex("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        .expect("the synthetic digest")
}

// ============================================================
// Precision goldens — the portable/GridWork asymmetry
// ============================================================

#[test]
fn an_unknown_portable_field_is_kept_as_evidence_and_an_unknown_gridwork_field_is_fatal() {
    // The asymmetry R13 asks for, as one comparison rather than two tests that
    // could drift apart. Upstream shipping a field this parser predates is not
    // an attack; a caller inventing GridWork contract is.
    let portable =
        manifest_with("name: fixture-asymmetry\ndescription: d\ninvented-by-upstream: someday")
            .expect("an unknown portable field is not an error");
    assert_eq!(portable.opaque.len(), 1);
    assert_eq!(portable.opaque[0].key, "invented-by-upstream");

    let gridwork = manifest_with(
        "name: fixture-asymmetry\ndescription: d\nmetadata:\n  gridwork: '{\"routes\": [], \"invented\": true}'",
    )
    .expect_err("an unknown GridWork field must be fatal");
    assert!(
        matches!(gridwork, SkillError::InvalidGridworkExtension(_)),
        "{gridwork:?}"
    );

    // And the dropped third option: the portable field is neither refused NOR
    // silently discarded. A dropped field is a difference between what the
    // author wrote and what the compiler saw, with nothing recording it.
    assert_eq!(portable.opaque[0].value, "someday");
}

#[test]
fn a_duplicate_key_is_refused_rather_than_resolved_by_preference() {
    // Last-one-wins is how several YAML libraries handle this, silently. The
    // pinned one does: the pre-parse subset gate is what refuses it, which is
    // why that gate runs before the deserializer rather than after.
    let error = manifest_with("name: fixture-a\nname: fixture-b\ndescription: d")
        .expect_err("a duplicate key must be refused");
    assert!(
        matches!(&error, SkillError::DuplicateKey(k) if k == "name"),
        "{error:?}"
    );
}

#[test]
fn allowed_tools_is_evidence_and_intersecting_is_the_whole_mitigation() {
    // Upstream calls this field experimental and does not define it as a grant.
    // Treating a manifest's own claim as authority is the entire attack, so the
    // type exposes `claimed()` and no conversion into anything grant-shaped.
    let manifest = manifest_with(
        "name: fixture-tools\ndescription: d\nallowed-tools:\n  - read\n  - write\n  - deploy",
    )
    .expect("parses");
    assert_eq!(manifest.allowed_tools.claimed().len(), 3);

    // The intersection, spelled out here rather than trusted to the crate's own
    // helper, because this test's job is to notice if that helper changes.
    let granted: BTreeSet<&str> = ["read", "audit"].into_iter().collect();
    let effective: BTreeSet<&str> = manifest
        .allowed_tools
        .claimed()
        .iter()
        .map(String::as_str)
        .filter(|t| granted.contains(t))
        .collect();

    assert_eq!(effective, ["read"].into_iter().collect::<BTreeSet<_>>());
    assert!(
        !effective.contains("deploy"),
        "a manifest widened the authority it was handed"
    );
    // Both directions: the grant is not widened by the claim, and the claim is
    // not widened by the grant. Asserting one alone passes a union.
    assert!(
        !effective.contains("audit"),
        "an intersection produced a tool the manifest never claimed"
    );
}

// ============================================================
// Precision golden — attribution cannot be spoofed into a manifest
// ============================================================

#[test]
fn no_truth_record_offers_a_field_a_client_supplied_actor_could_land_in() {
    // CTX-12's manifest-side half. The wire-side control is that the command
    // shape carries no actor; this is the reason that control is sufficient —
    // there is no field on the record for one to reach even if it did.
    //
    // Asserted over the SERIALIZED form, because a field added under a serde
    // rename is still a field on the wire, and asserting on the Rust struct
    // would miss exactly that.
    let record = ResolvedManifest {
        id: gwk_context::ManifestId::parse("fixture-manifest").expect("valid"),
        attempt_id: gwk_domain::AttemptId::new("fixture-attempt"),
        manifest_digest: digest(),
        route_digest: digest(),
        authority_digest: digest(),
        source_count: gwk_context::RecordCount::new(1).expect("valid"),
        source_bytes: gwk_domain::ByteCount::new(1),
        participations: gwk_context::ParticipationRecords::new(vec![Participation::active(
            digest(),
        )])
        .expect("valid"),
        evidence_ids: EvidenceRefs::new(Vec::new()).expect("valid"),
        resolved_at: gwk_domain::Timestamp::new("2026-08-14T00:00:00Z"),
    };

    // Compared against KEYS, not against the serialized text. A substring
    // search is what this test did first, and `author` matched inside
    // `authority_digest` — a false positive that would have been "fixed" by
    // dropping `author` from the list, quietly deleting the check for the one
    // field name a spoofed manifest is most likely to use.
    let value = serde_json::to_value(&record).expect("serializes");
    let keys = collect_keys(&value);
    assert!(
        !keys.is_empty(),
        "the record serialized no keys at all; this test is inspecting nothing"
    );

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
    // The count first: an empty forbidden list would make the loop below pass
    // over any record at all.
    assert_eq!(forbidden.len(), 8);
    for key in forbidden {
        assert!(
            !keys.contains(key),
            "a resolved manifest carries `{key}`, which a client could then supply: {value}"
        );
    }

    // The positive control. If key collection were broken, every assertion
    // above would pass by not looking.
    assert!(
        keys.contains("manifest_digest"),
        "the record did not serialize the fields it does have: {keys:?}"
    );
    // Nested objects are reached too — participation rows are where a
    // per-source attribution field would most plausibly be added.
    assert!(
        keys.contains("state"),
        "nested participation keys were not collected: {keys:?}"
    );
}

/// Every object key anywhere in a JSON value, including inside arrays.
///
/// Recursive because a spoofed field does not have to be top level, and a check
/// that only reads the outermost object is one nesting level from useless.
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
// Generated battery — every upstream bound, at the edge and one past it
// ============================================================

/// One bound: a frontmatter that sits exactly at it, and one that exceeds it.
///
/// `refusal` is per case rather than one blanket `matches!` across all of them.
/// An over-long name is refused as `InvalidName` — the name's charset and length
/// are one validity question, not two — and a blanket assertion that accepted
/// `FieldTooLong | TooManyEntries` would have had to be widened until it
/// accepted everything to make that case pass.
struct BoundCase {
    label: &'static str,
    at_edge: String,
    past_edge: String,
    refusal: fn(&SkillError) -> bool,
}

/// `n` unrecognized top-level fields, each inside every per-field limit, so the
/// only bound the document can trip is the opaque field COUNT.
fn opaque_fields(n: usize) -> String {
    let mut front = String::from("name: fixture-bounds\ndescription: d");
    for i in 0..n {
        front.push_str(&format!("\nopaque-{i}: v"));
    }
    front
}

fn bound_cases() -> Vec<BoundCase> {
    let base = "name: fixture-bounds\ndescription: d";
    vec![
        BoundCase {
            // `SKILL_OPAQUE_FIELD_MAX_COUNT` had no test reference anywhere.
            label: "unrecognized fields",
            at_edge: opaque_fields(SKILL_OPAQUE_FIELD_MAX_COUNT),
            past_edge: opaque_fields(SKILL_OPAQUE_FIELD_MAX_COUNT + 1),
            refusal: |e| matches!(e, SkillError::TooManyEntries("unrecognized fields")),
        },
        BoundCase {
            label: "name",
            at_edge: format!("name: {}\ndescription: d", "a".repeat(SKILL_NAME_MAX_BYTES)),
            past_edge: format!(
                "name: {}\ndescription: d",
                "a".repeat(SKILL_NAME_MAX_BYTES + 1)
            ),
            refusal: |e| matches!(e, SkillError::InvalidName),
        },
        BoundCase {
            label: "description",
            at_edge: format!(
                "name: fixture-bounds\ndescription: {}",
                "d".repeat(SKILL_DESCRIPTION_MAX_BYTES)
            ),
            past_edge: format!(
                "name: fixture-bounds\ndescription: {}",
                "d".repeat(SKILL_DESCRIPTION_MAX_BYTES + 1)
            ),
            refusal: |e| matches!(e, SkillError::FieldTooLong("description")),
        },
        BoundCase {
            label: "license",
            at_edge: format!("{base}\nlicense: {}", "L".repeat(SKILL_LICENSE_MAX_BYTES)),
            past_edge: format!(
                "{base}\nlicense: {}",
                "L".repeat(SKILL_LICENSE_MAX_BYTES + 1)
            ),
            refusal: |e| matches!(e, SkillError::FieldTooLong("license")),
        },
        BoundCase {
            label: "compatibility",
            at_edge: format!(
                "{base}\ncompatibility: {}",
                "c".repeat(SKILL_COMPATIBILITY_MAX_BYTES)
            ),
            past_edge: format!(
                "{base}\ncompatibility: {}",
                "c".repeat(SKILL_COMPATIBILITY_MAX_BYTES + 1)
            ),
            refusal: |e| matches!(e, SkillError::FieldTooLong("compatibility")),
        },
        BoundCase {
            label: "metadata key",
            at_edge: format!(
                "{base}\nmetadata:\n  {}: v",
                "k".repeat(SKILL_METADATA_KEY_MAX_BYTES)
            ),
            past_edge: format!(
                "{base}\nmetadata:\n  {}: v",
                "k".repeat(SKILL_METADATA_KEY_MAX_BYTES + 1)
            ),
            refusal: |e| matches!(e, SkillError::FieldTooLong("metadata key")),
        },
        BoundCase {
            label: "metadata value",
            at_edge: format!(
                "{base}\nmetadata:\n  k: {}",
                "v".repeat(SKILL_METADATA_VALUE_MAX_BYTES)
            ),
            past_edge: format!(
                "{base}\nmetadata:\n  k: {}",
                "v".repeat(SKILL_METADATA_VALUE_MAX_BYTES + 1)
            ),
            refusal: |e| matches!(e, SkillError::FieldTooLong("metadata value")),
        },
        BoundCase {
            label: "metadata entries",
            at_edge: format!(
                "{base}\nmetadata:\n{}",
                (0..SKILL_METADATA_MAX_ENTRIES)
                    .map(|i| format!("  k{i}: v\n"))
                    .collect::<String>()
                    .trim_end()
            ),
            past_edge: format!(
                "{base}\nmetadata:\n{}",
                (0..SKILL_METADATA_MAX_ENTRIES + 1)
                    .map(|i| format!("  k{i}: v\n"))
                    .collect::<String>()
                    .trim_end()
            ),
            refusal: |e| matches!(e, SkillError::TooManyEntries("metadata")),
        },
        BoundCase {
            label: "allowed-tools count",
            at_edge: format!(
                "{base}\nallowed-tools:\n{}",
                (0..SKILL_ALLOWED_TOOLS_MAX_COUNT)
                    .map(|i| format!("  - t{i}\n"))
                    .collect::<String>()
                    .trim_end()
            ),
            past_edge: format!(
                "{base}\nallowed-tools:\n{}",
                (0..SKILL_ALLOWED_TOOLS_MAX_COUNT + 1)
                    .map(|i| format!("  - t{i}\n"))
                    .collect::<String>()
                    .trim_end()
            ),
            refusal: |e| matches!(e, SkillError::TooManyEntries("allowed-tools")),
        },
        BoundCase {
            label: "allowed-tool length",
            at_edge: format!(
                "{base}\nallowed-tools:\n  - {}",
                "t".repeat(SKILL_ALLOWED_TOOL_MAX_BYTES)
            ),
            past_edge: format!(
                "{base}\nallowed-tools:\n  - {}",
                "t".repeat(SKILL_ALLOWED_TOOL_MAX_BYTES + 1)
            ),
            refusal: |e| matches!(e, SkillError::FieldTooLong("allowed-tools")),
        },
    ]
}

#[test]
fn every_upstream_bound_accepts_its_edge_and_refuses_one_past_it() {
    let cases = bound_cases();

    // The count first. A generator that returned an empty vector would make
    // every assertion below vacuous, and the loop would report success.
    assert!(
        cases.len() >= 10,
        "expected at least 10 bound cases, generated {}",
        cases.len()
    );

    let mut checked = 0usize;
    for case in &cases {
        // The edge is ACCEPTED. Without this half, a parser that refused
        // everything would pass the refusal half of every case.
        manifest_with(&case.at_edge).unwrap_or_else(|e| {
            panic!("{}: the exact bound was refused: {e:?}", case.label);
        });
        let error = manifest_with(&case.past_edge)
            .err()
            .unwrap_or_else(|| panic!("{}: one past the bound was accepted", case.label));
        assert!(
            (case.refusal)(&error),
            "{}: refused for the wrong reason: {error:?}",
            case.label
        );
        checked += 1;
    }
    assert_eq!(checked, cases.len());
}

#[test]
fn the_whole_document_bounds_refuse_before_parsing() {
    // Not per-field: these two bound the input itself, so they must fire before
    // any structure is read. A parser that checked them afterwards has already
    // allocated whatever it was handed.
    let oversized = "x".repeat(SKILL_INPUT_MAX_BYTES + 1);
    assert!(matches!(
        SkillManifest::parse(&oversized),
        Err(SkillError::InputTooLarge)
    ));

    // Legal in every other respect, so only the document bound can refuse it.
    // The previous spelling put an 8,192-byte `description` here — eight times
    // past the description bound — and asserted
    // `FrontmatterTooLarge | FieldTooLong(_)`, so the second arm answered and
    // deleting the byte bound entirely left the suite green. `FieldTooLong` is
    // also precisely the outcome proving structure WAS read, which is what the
    // comment above forbids.
    let head = "name: fixture-a\ndescription: d\npadding: ";
    let big_front = format!("{head}{}", "x".repeat(SKILL_FRONTMATTER_MAX_BYTES));
    assert!(big_front.len() > SKILL_FRONTMATTER_MAX_BYTES);
    assert!(matches!(
        manifest_with(&big_front),
        Err(SkillError::FrontmatterTooLarge)
    ));

    // The accepted edge, which the neighbouring battery models for all eight of
    // its cases and this bound did not have. `padding` is an unrecognized
    // portable field, so it is kept as opaque evidence and truncated rather
    // than refused — nothing but the document bound can speak here.
    let at_edge = format!(
        "{head}{}",
        "x".repeat(SKILL_FRONTMATTER_MAX_BYTES - head.len())
    );
    assert_eq!(at_edge.len(), SKILL_FRONTMATTER_MAX_BYTES);
    manifest_with(&at_edge).expect("a frontmatter of exactly the bound is accepted");
}

// ============================================================
// Generated battery — bundle entries
// ============================================================

#[test]
fn every_bundle_shape_that_is_not_declarative_content_is_refused() {
    // Refusal is the default in `classify`, so this battery is checking that
    // the default is reachable by every route someone would actually take.
    // The path, the kind its caller reported, and the refusal it must produce —
    // matched on the variant rather than the message, so rewording a `Display`
    // arm does not red this battery.
    type BundleCase = (&'static str, RawEntryKind, fn(&BundleRefusal) -> bool);

    let cases: Vec<BundleCase> = vec![
        (
            "../escape.md",
            RawEntryKind::File { executable: false },
            |r| matches!(r, BundleRefusal::ParentTraversal),
        ),
        (
            "nested/../../escape.md",
            RawEntryKind::File { executable: false },
            |r| matches!(r, BundleRefusal::ParentTraversal),
        ),
        (
            "/etc/passwd",
            RawEntryKind::File { executable: false },
            |r| matches!(r, BundleRefusal::AbsolutePath),
        ),
        (
            "C:/windows/system32",
            RawEntryKind::File { executable: false },
            |r| matches!(r, BundleRefusal::AbsolutePath),
        ),
        (
            "dir\\file.md",
            RawEntryKind::File { executable: false },
            |r| matches!(r, BundleRefusal::MalformedPath),
        ),
        (
            "bell\u{0007}.md",
            RawEntryKind::File { executable: false },
            |r| matches!(r, BundleRefusal::MalformedPath),
        ),
        ("", RawEntryKind::File { executable: false }, |r| {
            matches!(r, BundleRefusal::MalformedPath)
        }),
        (
            "run.sh",
            RawEntryKind::File { executable: false },
            |r| matches!(r, BundleRefusal::ExecutableSuffix(s) if s == "sh"),
        ),
        (
            "payload.exe",
            RawEntryKind::File { executable: false },
            |r| matches!(r, BundleRefusal::ExecutableSuffix(s) if s == "exe"),
        ),
        (
            "SHOUT.PY",
            RawEntryKind::File { executable: false },
            |r| matches!(r, BundleRefusal::ExecutableSuffix(s) if s == "py"),
        ),
        (
            "innocent.md",
            RawEntryKind::File { executable: true },
            |r| matches!(r, BundleRefusal::Executable),
        ),
        ("link.md", RawEntryKind::Symlink, |r| {
            matches!(r, BundleRefusal::NotARegularFile)
        }),
        ("subdir", RawEntryKind::Directory, |r| {
            matches!(r, BundleRefusal::NotARegularFile)
        }),
        ("device", RawEntryKind::Other, |r| {
            matches!(r, BundleRefusal::NotARegularFile)
        }),
        (
            "archive.tar",
            RawEntryKind::File { executable: false },
            |r| matches!(r, BundleRefusal::Unclassified(s) if s == "tar"),
        ),
        (
            "no-suffix",
            RawEntryKind::File { executable: false },
            |r| matches!(r, BundleRefusal::Unclassified(s) if s.is_empty()),
        ),
    ];

    assert!(
        cases.len() >= 16,
        "expected at least 16 bundle cases, wrote {}",
        cases.len()
    );

    let mut checked = 0usize;
    for (path, kind, matches_refusal) in &cases {
        let error = BundleEntry::classify(path, *kind)
            .err()
            .unwrap_or_else(|| panic!("{path:?} was accepted into a bundle"));
        let SkillError::RefusedBundleEntry(refusal) = &error else {
            panic!("{path:?} refused as {error:?}, not as a bundle refusal");
        };
        assert!(matches_refusal(refusal), "{path:?} refused as {refusal:?}");
        checked += 1;
    }
    assert_eq!(checked, cases.len());

    // The positive control. Every case above is a refusal, so a `classify` that
    // refused unconditionally would pass all of them — and this repo has the
    // rule about a grep that matches nothing anywhere.
    for (path, expected) in [
        ("guide.md", gwk_context::skill::BundleEntryKind::Reference),
        ("data.json", gwk_context::skill::BundleEntryKind::Data),
        ("notes.txt", gwk_context::skill::BundleEntryKind::Text),
    ] {
        let entry = BundleEntry::classify(path, RawEntryKind::File { executable: false })
            .unwrap_or_else(|e| panic!("{path} is declarative content but was refused: {e:?}"));
        assert_eq!(entry.kind, expected);
    }
}

#[test]
fn a_bundle_refuses_past_its_entry_count() {
    let ok: Vec<(String, RawEntryKind)> = (0..SKILL_BUNDLE_MAX_ENTRIES)
        .map(|i| (format!("f{i}.md"), RawEntryKind::File { executable: false }))
        .collect();
    let inventory =
        BundleEntry::inventory(ok.iter().map(|(p, k)| (p.as_str(), *k))).expect("at the bound");
    assert_eq!(inventory.len(), SKILL_BUNDLE_MAX_ENTRIES);

    let too_many: Vec<(String, RawEntryKind)> = (0..SKILL_BUNDLE_MAX_ENTRIES + 1)
        .map(|i| (format!("f{i}.md"), RawEntryKind::File { executable: false }))
        .collect();
    assert!(matches!(
        BundleEntry::inventory(too_many.iter().map(|(p, k)| (p.as_str(), *k))),
        Err(SkillError::TooManyEntries("bundle"))
    ));
}

#[test]
fn a_bundle_path_refuses_past_its_byte_bound() {
    let at_edge = format!("{}.md", "a".repeat(SKILL_BUNDLE_PATH_MAX_BYTES - 3));
    assert_eq!(at_edge.len(), SKILL_BUNDLE_PATH_MAX_BYTES);
    BundleEntry::classify(&at_edge, RawEntryKind::File { executable: false })
        .expect("the exact bound is allowed");

    let past = format!("{}.md", "a".repeat(SKILL_BUNDLE_PATH_MAX_BYTES - 2));
    assert!(matches!(
        BundleEntry::classify(&past, RawEntryKind::File { executable: false }),
        Err(SkillError::RefusedBundleEntry(BundleRefusal::MalformedPath))
    ));
}

// ============================================================
// Precision goldens — evidence and participation fail closed
// ============================================================
//
// The precedence golden that sat here — an equal-tier disagreement fails closed
// rather than picking — moved with the resolver to `gwk-context-compiler`'s
// suite: the function it exercises no longer lives in this crate (R15).

#[test]
fn oversized_evidence_fails_closed_at_the_vocabulary_boundary() {
    let at_edge: Vec<EvidenceId> = (0..CONTEXT_EVIDENCE_MAX_COUNT)
        .map(|i| EvidenceId::new(format!("fixture-evidence-{i}")))
        .collect();
    assert_eq!(
        EvidenceRefs::new(at_edge)
            .expect("the exact bound")
            .as_slice()
            .len(),
        CONTEXT_EVIDENCE_MAX_COUNT
    );

    let past: Vec<EvidenceId> = (0..CONTEXT_EVIDENCE_MAX_COUNT + 1)
        .map(|i| EvidenceId::new(format!("fixture-evidence-{i}")))
        .collect();
    assert_eq!(
        EvidenceRefs::new(past),
        Err(TruthRecordError::TooManyEvidenceRefs)
    );
}

#[test]
fn a_participation_that_claims_a_state_without_its_reason_is_refused() {
    // The truth table, both directions. A state that requires a reason and has
    // none is incomplete; a state that forbids one and has it is a caller
    // asserting something the state does not mean.
    let missing = Participation {
        digest: digest(),
        state: gwk_context::ParticipationState::Excluded,
        reason: None,
        detail: None,
    };
    assert_eq!(
        missing.validate(),
        Err(gwk_context::ParticipationError::MissingReason)
    );

    let unexpected = Participation {
        digest: digest(),
        state: gwk_context::ParticipationState::Active,
        reason: Some(ParticipationReason::BudgetCut),
        detail: None,
    };
    assert_eq!(
        unexpected.validate(),
        Err(gwk_context::ParticipationError::UnexpectedReason)
    );

    // The positive control: a well-formed record of each kind validates, so the
    // two assertions above are not passing because `validate` refuses all input.
    assert_eq!(Participation::active(digest()).validate(), Ok(()));
    assert_eq!(
        Participation::excluded(digest(), ParticipationReason::Quarantined).validate(),
        Ok(())
    );
}
