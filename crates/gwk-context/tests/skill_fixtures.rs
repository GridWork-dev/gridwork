//! The skill-fixture catalog, walked rather than listed.
//!
//! `include_str!` would be simpler and would prove less. A fixture added to the
//! tree and never named in a test is invisible to a compile-time include: the
//! suite stays green and the case nobody runs looks exactly like the case that
//! passes. So this reads the directory at run time and requires the two sets to
//! agree in both directions — a file with no declared expectation reds, and a
//! declared expectation with no file reds.
//!
//! It also asserts the count before anything else. A walk over a directory that
//! moved returns an empty iterator, every `for` body is skipped, and a suite
//! that inspected nothing reports the same success as one that inspected
//! everything.

use std::collections::BTreeMap;
use std::path::Path;

use gwk_context::skill::{SkillError, SkillManifest};

const SKILLS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/skills");

/// What one fixture must do: `None` parses, `Some(pred)` is refused by shape.
///
/// A predicate rather than an `Eq` comparison because the refusal is matched on
/// its variant, not its message — rewording a `Display` arm should not red the
/// suite, and asserting on prose would make it.
type Expectation = Option<fn(&SkillError) -> bool>;

/// The declared outcome of every fixture, keyed by `<dir>/<file>`.
type Catalog = BTreeMap<&'static str, Expectation>;

fn catalog() -> Catalog {
    let mut c: Catalog = BTreeMap::new();
    c.insert("accepted/minimal.md", None);
    c.insert("accepted/full-portable.md", None);
    c.insert("accepted/unknown-portable-field.md", None);
    c.insert("accepted/gridwork-extension.md", None);
    c.insert(
        "refused/duplicate-key.md",
        Some(|e| matches!(e, SkillError::DuplicateKey(k) if k == "name")),
    );
    c.insert(
        "refused/anchor.md",
        Some(|e| matches!(e, SkillError::UnsupportedYaml("anchor"))),
    );
    c.insert(
        "refused/alias.md",
        Some(|e| matches!(e, SkillError::UnsupportedYaml("alias"))),
    );
    c.insert(
        "refused/merge-key.md",
        Some(|e| matches!(e, SkillError::UnsupportedYaml("merge key"))),
    );
    c.insert(
        "refused/tab-indent.md",
        Some(|e| matches!(e, SkillError::TabIndentation)),
    );
    c.insert(
        "refused/non-string-metadata.md",
        Some(|e| matches!(e, SkillError::MetadataValueNotString(k) if k == "count")),
    );
    c.insert(
        "refused/unknown-gridwork-key.md",
        Some(|e| matches!(e, SkillError::InvalidGridworkExtension(_))),
    );
    c.insert(
        "refused/invalid-name.md",
        Some(|e| matches!(e, SkillError::InvalidName)),
    );
    c.insert(
        "refused/no-frontmatter.md",
        Some(|e| matches!(e, SkillError::MissingFrontmatter)),
    );
    // The flow-style axis. `refused/{anchor,alias,merge-key}.md` are all block
    // style, and every one of those properties was reachable in flow style, so
    // the corpus reported three refusals it could not tell from three escapes.
    c.insert("accepted/flow-allowed-tools.md", None);
    c.insert(
        "refused/flow-mapping.md",
        Some(|e| matches!(e, SkillError::UnsupportedYaml("flow mapping"))),
    );
    c.insert(
        "refused/flow-sequence-alias.md",
        Some(|e| matches!(e, SkillError::UnsupportedYaml("alias"))),
    );
    c.insert(
        "refused/deep-nesting.md",
        Some(|e| matches!(e, SkillError::TooDeeplyNested)),
    );
    // The offset axis. Every flow case above sits in the top-level
    // `key: <flow>` position, where a head-of-node test lands on the collection
    // by positional luck; each of these moves the indicator off that offset,
    // and all three were accepted until the scan stopped deriving one candidate
    // slice from the first `": "`.
    c.insert(
        "refused/flow-in-sequence-item.md",
        Some(|e| matches!(e, SkillError::UnsupportedYaml("flow mapping"))),
    );
    c.insert(
        "refused/quoted-key.md",
        Some(|e| matches!(e, SkillError::MalformedTopLevelKey)),
    );
    c.insert(
        "refused/tab-separator.md",
        Some(|e| matches!(e, SkillError::UnsupportedYaml("anchor"))),
    );
    // The line axis. Every fixture above is one line per construct, so the whole
    // corpus could only ever exercise a first line — and the scan is handed each
    // line with its state reset. All three of these were accepted, and in all
    // three the parser resolved an alias across two top-level keys.
    c.insert(
        "refused/multi-line-flow.md",
        Some(|e| matches!(e, SkillError::UnsupportedYaml("multi-line flow collection"))),
    );
    c.insert(
        "refused/apostrophe-anchor.md",
        Some(|e| matches!(e, SkillError::UnsupportedYaml("anchor"))),
    );
    c.insert(
        "refused/explicit-key.md",
        Some(|e| matches!(e, SkillError::UnsupportedYaml("explicit key"))),
    );
    // The accepted side of the same axis: content that spans lines and must NOT
    // be read as YAML, so the refusals above are not paid for by refusing prose.
    c.insert("accepted/block-scalar-prose.md", None);
    // The column axis. Every block scalar above sits at column zero — the one
    // position where the header line's indentation and the owning node's column
    // coincide — so the corpus could not see a skip armed at the wrong floor.
    // The item axis. Every accepted sequence above holds plain scalars, so the
    // corpus could not tell a depth bound from an item counter: a sequence of
    // mappings was accepted at one entry and refused at two.
    c.insert("accepted/sequence-of-mappings.md", None);
    c.insert(
        "refused/block-scalar-in-sequence-item.md",
        Some(|e| {
            matches!(
                e,
                SkillError::UnsupportedYaml("block scalar behind a sequence marker")
            )
        }),
    );
    c
}

fn walk() -> Vec<String> {
    let mut found = Vec::new();
    for group in ["accepted", "refused"] {
        let dir = Path::new(SKILLS_DIR).join(group);
        let entries = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("fixture directory {} is unreadable: {e}", dir.display()));
        for entry in entries {
            let entry = entry.expect("directory entry");
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".md") {
                found.push(format!("{group}/{name}"));
            }
        }
    }
    found.sort();
    found
}

#[test]
fn every_fixture_on_disk_is_exercised_and_every_expectation_has_a_fixture() {
    let found = walk();
    let catalog = catalog();

    // The count first. Everything below is a fold, and a fold over nothing
    // agrees with itself.
    assert!(
        found.len() >= 26,
        "expected at least 26 skill fixtures, walked {}",
        found.len()
    );
    assert_eq!(
        found.len(),
        catalog.len(),
        "fixture files on disk: {found:?}\ndeclared: {:?}",
        catalog.keys().collect::<Vec<_>>()
    );
    for name in &found {
        assert!(
            catalog.contains_key(name.as_str()),
            "{name} is on disk with no declared expectation"
        );
    }
}

#[test]
fn each_fixture_does_what_the_catalog_says() {
    let catalog = catalog();
    let mut checked = 0usize;

    for (name, expectation) in &catalog {
        let path = Path::new(SKILLS_DIR).join(name);
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("{} is unreadable: {e}", path.display()));
        let outcome = SkillManifest::parse(&raw);
        match expectation {
            None => {
                let manifest =
                    outcome.unwrap_or_else(|e| panic!("{name} was expected to parse, got {e:?}"));
                // A fixture's own name is its declared identity; the parser
                // must agree with it, which also proves the file was read.
                let stem = name
                    .rsplit_once('/')
                    .map(|(_, f)| f.trim_end_matches(".md"))
                    .expect("group-qualified name");
                if stem != "minimal" {
                    assert_eq!(manifest.name, stem, "{name}");
                }
            }
            Some(matches_refusal) => {
                let err = outcome
                    .err()
                    .unwrap_or_else(|| panic!("{name} was expected to be refused"));
                assert!(matches_refusal(&err), "{name} refused as {err:?}");
            }
        }
        checked += 1;
    }

    assert_eq!(checked, catalog.len());
    assert!(checked > 0, "the catalog inspected nothing");
}

#[test]
fn the_accepted_fixtures_carry_the_evidence_the_asymmetry_promises() {
    let opaque =
        std::fs::read_to_string(Path::new(SKILLS_DIR).join("accepted/unknown-portable-field.md"))
            .expect("readable");
    let manifest = SkillManifest::parse(&opaque).expect("parses");
    assert_eq!(manifest.opaque.len(), 1);
    assert_eq!(manifest.opaque[0].key, "invocation-hint");
    assert_eq!(manifest.opaque[0].value, "proactive");

    // The other half of the asymmetry, and the half nothing asserted: a
    // manifest whose every key is in `PORTABLE_CORE_FIELDS` puts NOTHING in the
    // evidence bucket. `full-portable.md` has claimed exactly that in its own
    // prose since it was written, and only `manifest.name` was ever checked —
    // so dropping a member from `PORTABLE_CORE_FIELDS` left the suite green
    // while that field quietly became opaque evidence instead of a typed read.
    let full = std::fs::read_to_string(Path::new(SKILLS_DIR).join("accepted/full-portable.md"))
        .expect("readable");
    let manifest = SkillManifest::parse(&full).expect("parses");
    assert!(
        manifest.opaque.is_empty(),
        "every key is portable-core, so nothing is evidence: {:?}",
        manifest.opaque
    );
    assert_eq!(manifest.license.as_deref(), Some("Apache-2.0"));
    assert_eq!(manifest.compatibility.as_deref(), Some(">=1.0"));
    let claimed: Vec<&str> = manifest
        .allowed_tools
        .claimed()
        .iter()
        .map(String::as_str)
        .collect();
    assert_eq!(claimed, ["Read", "Grep"]);
    assert_eq!(
        manifest.metadata.get("team").map(String::as_str),
        Some("infra")
    );

    let ext = std::fs::read_to_string(Path::new(SKILLS_DIR).join("accepted/gridwork-extension.md"))
        .expect("readable");
    let manifest = SkillManifest::parse(&ext).expect("parses");
    let gridwork = manifest.gridwork.expect("extension decoded");
    assert_eq!(gridwork.routes, ["code_review"]);
    assert_eq!(gridwork.budget_tokens, Some(8192));
    assert!(!manifest.metadata.contains_key("gridwork"));
    assert_eq!(
        manifest.metadata.get("team").map(String::as_str),
        Some("infra")
    );
}
