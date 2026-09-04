//! R15, enforced rather than remembered.
//!
//! The verifier's independence from the compiler is a property of the crate
//! graph. This test is what makes that property fail loudly instead of decaying
//! the first time someone reaches for a convenient import.
//!
//! # Why this parses TOML instead of scanning lines
//!
//! The sibling guard in `gwk-context` learned half of this lesson already: its
//! workspace-manifest reader was a line scanner that recognised exactly one
//! spelling, so `path='…'` and every other legal quoting was invisible, and it
//! was rewritten to parse real TOML. The other half is still worth stating,
//! because the same file's crate-manifest reader was left line-based and shows
//! what that costs: it enters a table only when a header line is exactly
//! `[dependencies]`, so `[dev-dependencies]`, `[build-dependencies]`,
//! `[target.'cfg(unix)'.dependencies]`, and even the table-form
//! `[dependencies.gwk-context-compiler]` all read as nothing at all.
//!
//! That matters here more than it did there. A dev-dependency is not a lesser
//! dependency for this purpose — a test target promotes it into the graph, and
//! a test that could call the compiler is exactly the collusion R15 forbids.
//! So this guard reads every dependency table there is, through a real parser,
//! and the mutation that has to red is one that ADDS an edge in its least
//! obvious spelling rather than one that removes an obvious one.
//!
//! # Why set equality rather than "the forbidden crate is absent"
//!
//! A denylist fails at the same step that already failed: it catches the edge
//! someone thought to forbid and waves through the next one. Asserting the
//! whole set means a new dependency of any name stops this test until someone
//! writes down why it is allowed — which is the decision R15 wants made out
//! loud, not a review remembering to look.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Every dependency this crate may take at build or run time.
const ALLOWED_RUNTIME: &[&str] = &["gwk-context", "gwk-domain", "serde_json", "sha2"];

/// Every dependency this crate may take for its own tests.
const ALLOWED_DEV: &[&str] = &["toml"];

/// Named for the failure message, not for the check: set equality above already
/// refuses these. Naming them is how the failure says *R15* rather than "the
/// dependency set moved".
const FORBIDDEN: &[&str] = &["gwk-context-compiler", "gwk-kernel"];

/// The three kinds of dependency table Cargo honours, at the top level and
/// again under every `[target.*]` entry.
const KINDS: &[&str] = &["dependencies", "dev-dependencies", "build-dependencies"];

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn workspace_root() -> PathBuf {
    crate_dir()
        .parent()
        .and_then(Path::parent)
        .expect("crate sits two levels below the workspace root")
        .to_path_buf()
}

fn parse(path: &Path) -> toml::Table {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("{} is readable: {e}", path.display()));
    text.parse()
        .unwrap_or_else(|e| panic!("{} is TOML: {e}", path.display()))
}

/// What one manifest declares, by kind, plus how many tables were actually
/// inspected to find it.
///
/// The count is returned rather than kept internal because the caller asserts
/// on it before it folds: a walker that visited nothing produces an empty set,
/// and an empty set satisfies every containment check ever written.
fn declared(manifest: &toml::Table) -> (BTreeMap<String, BTreeSet<String>>, usize) {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut tables = 0usize;

    let mut take = |kind: &str, table: &toml::Value| {
        if let Some(entries) = table.as_table() {
            tables += 1;
            let bucket = out.entry(kind.to_owned()).or_default();
            for name in entries.keys() {
                bucket.insert(name.clone());
            }
        }
    };

    for kind in KINDS {
        if let Some(table) = manifest.get(*kind) {
            take(kind, table);
        }
    }

    // `[target.'cfg(...)'.dependencies]` and its dev/build siblings. A guard
    // that stops at the top level is defeated by one cfg expression.
    if let Some(targets) = manifest.get("target").and_then(toml::Value::as_table) {
        for entry in targets.values() {
            let Some(entry) = entry.as_table() else {
                continue;
            };
            for kind in KINDS {
                if let Some(table) = entry.get(*kind) {
                    take(kind, table);
                }
            }
        }
    }

    (out, tables)
}

fn union(declared: &BTreeMap<String, BTreeSet<String>>, kinds: &[&str]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for kind in kinds {
        if let Some(names) = declared.get(*kind) {
            out.extend(names.iter().cloned());
        }
    }
    out
}

fn expected(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|n| (*n).to_owned()).collect()
}

#[test]
fn the_verifier_depends_on_the_vocabulary_and_never_on_the_compiler() {
    let manifest = parse(&crate_dir().join("Cargo.toml"));

    // The subject, before anything is measured about it. A manifest parser
    // pointed at the wrong file returns a confident, clean, entirely
    // meaningless answer — identical in shape to a real pass.
    let name = manifest
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(toml::Value::as_str)
        .expect("the manifest declares a package name");
    assert_eq!(
        name, "gwk-context-verifier",
        "this guard measured the wrong crate's manifest"
    );

    let (declared, tables) = declared(&manifest);

    // The count first. Everything below folds over these sets, and a fold over
    // nothing succeeds exactly as quietly as a fold over something correct.
    assert!(
        tables >= 2,
        "inspected {tables} dependency tables — expected at least [dependencies] \
         and [dev-dependencies]; the walker is broken, not the manifest"
    );
    let runtime = union(&declared, &["dependencies", "build-dependencies"]);
    let dev = union(&declared, &["dev-dependencies"]);
    assert!(
        !runtime.is_empty(),
        "this crate declares no runtime dependencies — the parse is broken, not the manifest"
    );

    assert_eq!(
        runtime,
        expected(ALLOWED_RUNTIME),
        "the verifier's runtime dependency set moved. R15 fixes it at gwk-context's \
         public types plus declared crypto primitives; a change here is a change to \
         what the verifier is allowed to know"
    );
    assert_eq!(
        dev,
        expected(ALLOWED_DEV),
        "the verifier's dev-dependency set moved. A dev-dependency is not a lesser \
         dependency here: a test target promotes it into the crate graph, so an \
         edge added under [dev-dependencies] reaches the compiler exactly as well \
         as one added under [dependencies]"
    );

    // Redundant with the two assertions above, and kept for what it says when
    // it fires: the set-equality failure reports that a set moved, this one
    // reports which rule was broken.
    let every = union(&declared, KINDS);
    for forbidden in FORBIDDEN {
        assert!(
            !every.contains(*forbidden),
            "R15: the verifier declares a dependency on {forbidden}. Verifying a \
             result with the code that produced it verifies nothing"
        );
    }
}

#[test]
fn the_vocabulary_crate_cannot_reach_the_compiler_either() {
    // The one transitive path a manifest-level guard could otherwise miss.
    // `gwk-context` is the only first-party crate the verifier depends on, so
    // if it stays clear of the compiler, no two-hop route exists. (It cannot
    // depend on the compiler without a cycle — but "cargo would refuse it" is
    // an argument, and this is a measurement.)
    let manifest = parse(&workspace_root().join("crates/gwk-context/Cargo.toml"));

    let name = manifest
        .get("package")
        .and_then(|package| package.get("name"))
        .and_then(toml::Value::as_str)
        .expect("the manifest declares a package name");
    assert_eq!(
        name, "gwk-context",
        "this guard measured the wrong manifest"
    );

    let (declared, tables) = declared(&manifest);
    assert!(
        tables >= 1,
        "inspected {tables} dependency tables in gwk-context — the walker is broken"
    );
    let every = union(&declared, KINDS);
    assert!(
        !every.is_empty(),
        "gwk-context declares no dependencies — the parse is broken, not the manifest"
    );
    assert!(
        !every.contains("gwk-context-compiler"),
        "gwk-context depends on gwk-context-compiler, which puts the compiler two \
         hops from the verifier and defeats R15 without touching the verifier's \
         own manifest"
    );
}

/// The walker, against a manifest that uses every table it claims to read.
///
/// Both tests above run `declared` over a real manifest, and neither real
/// manifest has a `[target.*]` section. So the second half of the walk — the one
/// whose own comment says a guard stopping at the top level is defeated by one
/// cfg expression — had never executed. Round 4 deleted the whole block and the
/// crate stayed green at 18.
///
/// The consequence is the one that comment predicts. `gwk-context-compiler`
/// declared under `[target.'cfg(unix)'.dependencies]` is a dependency cargo
/// builds on every Unix host, and with the block gone it never reaches the set
/// the R15 assertions are made over. The forbidden-name check would pass, the
/// set-equality check would pass, and the verifier would depend on the compiler.
///
/// A synthetic manifest rather than a fixture on disk: the real manifests must
/// NOT grow a target section just so this walk has something to find.
#[test]
fn the_walk_reads_target_scoped_tables_and_not_only_the_top_level() {
    let manifest: toml::Table = "[package]\n\
                                 name = \"synthetic\"\n\n\
                                 [dependencies]\n\
                                 top-level = \"1\"\n\n\
                                 [target.'cfg(unix)'.dependencies]\n\
                                 unix-only = \"1\"\n\n\
                                 [target.'cfg(windows)'.dev-dependencies]\n\
                                 windows-dev = \"1\"\n\n\
                                 [target.'cfg(unix)'.build-dependencies]\n\
                                 unix-build = \"1\"\n"
        .parse()
        .expect("the synthetic manifest is TOML");

    let (found, tables) = declared(&manifest);
    assert_eq!(
        tables, 4,
        "one top-level table and three target-scoped ones must all be walked"
    );

    // Per kind, not as one union. A walk that found every name but filed the
    // target-scoped ones under the wrong kind would satisfy a union check, and
    // the caller reads runtime and dev as separate sets with different rules.
    assert_eq!(
        union(&found, &["dependencies"]),
        expected(&["top-level", "unix-only"])
    );
    assert_eq!(
        union(&found, &["dev-dependencies"]),
        expected(&["windows-dev"])
    );
    assert_eq!(
        union(&found, &["build-dependencies"]),
        expected(&["unix-build"])
    );

    // The case this guard exists for, stated as itself rather than left implicit
    // in the counts above.
    let hidden: toml::Table = "[package]\n\
                               name = \"synthetic\"\n\n\
                               [dependencies]\n\
                               gwk-context = \"1\"\n\n\
                               [target.'cfg(unix)'.dependencies]\n\
                               gwk-context-compiler = \"1\"\n"
        .parse()
        .expect("the synthetic manifest is TOML");
    let (hidden, _) = declared(&hidden);
    assert!(
        union(&hidden, KINDS).contains("gwk-context-compiler"),
        "a forbidden dependency hidden behind a cfg expression is invisible to R15"
    );
}
