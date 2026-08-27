//! Mechanical guards on the public seam: where this crate's dependencies come
//! from, and what a synthetic catalog fixture is allowed to look like.
//!
//! Both guards are pure functions over text, applied twice — once to the real
//! artifact, and once to a seeded violation that must be rejected. That shape is
//! deliberate. A guard that only ever runs against a clean tree is a guard whose
//! detection could have been deleted years ago, and every run since would have
//! reported the same green. Feeding it something it must refuse is the only
//! evidence that it is still looking.
//!
//! The repo states the trap these guard against: a fold cannot tell "summed to
//! zero" from "summed over nothing". So each guard returns the COUNT it
//! inspected and each caller asserts that count is non-zero before believing a
//! verdict. Zero inspected is a broken guard, never a clean subject.

use std::collections::BTreeSet;
use std::path::Path;

const CRATE_DIR: &str = env!("CARGO_MANIFEST_DIR");

fn workspace_root() -> &'static Path {
    // crates/gwk-context -> crates -> the public root.
    Path::new(CRATE_DIR)
        .parent()
        .and_then(Path::parent)
        .expect("the crate sits two levels under the workspace root")
}

// ============================================================
// Guard 1 — dependency sources
// ============================================================

/// Pull `[workspace.dependencies]` out of a workspace manifest — PARSED, not
/// line-matched.
///
/// The predecessor scanned lines and matched spellings, and ordinary TOML
/// walked past it twice over (carryover row 6): a table-form dependency
/// (`[workspace.dependencies.sneaky]`) flipped its `inside` flag off at the
/// sub-table header and was never seen at all, and its path arm knew exactly
/// one spelling (`path = "…"`), so `path='…'`, `path="…"` without spaces, and
/// every other legal quoting was invisible. The parser also drops comments for
/// free, which matters here: this workspace documents its pins in long comment
/// blocks that quote the very strings this guard rejects.
fn declared_dependencies(manifest: &str) -> Result<toml::Table, String> {
    let parsed: toml::Table = manifest
        .parse()
        .map_err(|e| format!("workspace manifest is not TOML: {e}"))?;
    let deps = parsed
        .get("workspace")
        .and_then(|workspace| workspace.get("dependencies"))
        .and_then(|dependencies| dependencies.as_table())
        .cloned()
        .unwrap_or_default();
    Ok(deps)
}

/// Refuse a dependency the public root cannot vouch for.
///
/// Two rules, and they fail for different reasons. A `git` source is a
/// dependency on a moving reference no lockfile pins to a reviewable artifact.
/// A `path` source outside `crates/` is a dependency on something that is not in
/// this repository at all — it builds here, on this machine, and nowhere else,
/// and the failure surfaces as a broken publish long after the commit that
/// caused it.
///
/// Returns the number of declarations inspected, so the caller can tell a clean
/// manifest from a manifest this function failed to read.
fn inspect_dependency_sources(manifest: &str) -> Result<usize, String> {
    let declared = declared_dependencies(manifest)?;
    if declared.is_empty() {
        return Err(
            "inspected 0 workspace dependency declarations — the guard is broken, not the manifest"
                .into(),
        );
    }
    for (name, value) in &declared {
        // A plain string (`serde = "1"`) is a registry version; only the
        // table form can carry a source.
        let Some(table) = value.as_table() else {
            continue;
        };
        if let Some(git) = table.get("git") {
            return Err(format!("{name} declares a git source: {git}"));
        }
        if let Some(path) = table.get("path") {
            let path = path.as_str().unwrap_or_default();
            if !path.starts_with("crates/") {
                return Err(format!(
                    "{name} declares a path source outside the public root: {path}"
                ));
            }
        }
    }
    Ok(declared.len())
}

/// Every dependency this crate names, from its own `[dependencies]` table.
fn own_dependency_names(manifest: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut inside = false;
    for raw in manifest.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            inside = line == "[dependencies]";
            continue;
        }
        if !inside || line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, _)) = line.split_once('=') else {
            continue;
        };
        // `serde.workspace = true` and `specta = { workspace = true, ... }`
        // are both declarations; the name is whatever precedes the first dot
        // or the equals sign.
        let name = key.trim().split('.').next().unwrap_or_default().trim();
        if !name.is_empty() {
            out.insert(name.to_owned());
        }
    }
    out
}

#[test]
fn every_dependency_this_crate_takes_comes_from_the_public_root() {
    let workspace = std::fs::read_to_string(workspace_root().join("Cargo.toml"))
        .expect("workspace manifest is readable");
    let inspected = inspect_dependency_sources(&workspace)
        .unwrap_or_else(|e| panic!("public dependency seam: {e}"));

    // The count first. Everything above folded over a parse, and a parse that
    // returned nothing would have folded over nothing just as quietly.
    assert!(
        inspected >= 10,
        "expected at least 10 workspace dependency declarations, inspected {inspected}"
    );

    let own = std::fs::read_to_string(Path::new(CRATE_DIR).join("Cargo.toml"))
        .expect("crate manifest is readable");
    let names = own_dependency_names(&own);
    assert!(
        !names.is_empty(),
        "this crate declares no dependencies — the parse is broken, not the manifest"
    );

    // And every one of them resolves to a declaration the guard above checked,
    // so a dependency added directly to this crate with its own git source
    // cannot slip past a guard that only reads the workspace table.
    let declared: BTreeSet<String> = declared_dependencies(&workspace)
        .expect("workspace manifest parses")
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    for name in &names {
        assert!(
            declared.contains(name),
            "{name} is not declared in the workspace table; its source is unchecked"
        );
    }
}

#[test]
fn the_dependency_guard_refuses_a_table_form_external_path() {
    // Carryover row 6's exact mutation: the spelling that walked past the
    // line-based scan entirely — the sub-table header flipped its `inside`
    // flag off, so this dependency was never inspected at all.
    let seeded = "[workspace.dependencies]\n\
                  serde = \"1\"\n\n\
                  [workspace.dependencies.local]\n\
                  path = \"../../elsewhere/local\"\n";
    let error =
        inspect_dependency_sources(seeded).expect_err("a table-form outside path must be refused");
    assert!(error.contains("outside the public root"), "{error}");
    assert!(error.contains("local"), "{error}");

    // And the quoting variants the old path arm could not see.
    let single_quoted = "[workspace.dependencies]\n\
                         serde = \"1\"\n\
                         local = { path = '../../elsewhere/local' }\n";
    let error = inspect_dependency_sources(single_quoted)
        .expect_err("a single-quoted outside path must be refused");
    assert!(error.contains("outside the public root"), "{error}");

    let no_spaces = "[workspace.dependencies]\n\
                     serde = \"1\"\n\
                     local = { path=\"../../elsewhere/local\" }\n";
    let error = inspect_dependency_sources(no_spaces)
        .expect_err("an unspaced outside path must be refused");
    assert!(error.contains("outside the public root"), "{error}");

    // The in-repo shape stays legal, spelled both ways.
    let legal = "[workspace.dependencies]\n\
                 serde = \"1\"\n\
                 gwk-domain = { path = \"crates/gwk-domain\", version = \"0.0.3\" }\n\n\
                 [workspace.dependencies.gwk-theme]\n\
                 path = \"crates/gwk-theme\"\n";
    assert_eq!(
        inspect_dependency_sources(legal).expect("legal manifest"),
        3
    );
}

#[test]
fn the_dependency_guard_refuses_a_git_source() {
    // Seeded violation. Without this the test above passes on a tree that is
    // clean AND on a tree where the git check was deleted.
    let seeded = "[workspace.dependencies]\n\
                  serde = \"1\"\n\
                  sneaky = { git = \"https://example.invalid/x\", branch = \"main\" }\n";
    let error = inspect_dependency_sources(seeded).expect_err("a git source must be refused");
    assert!(error.contains("git source"), "{error}");
    assert!(error.contains("sneaky"), "{error}");
}

#[test]
fn the_dependency_guard_refuses_a_path_outside_the_public_root() {
    let seeded = "[workspace.dependencies]\n\
                  serde = \"1\"\n\
                  local = { path = \"../../elsewhere/local\" }\n";
    let error = inspect_dependency_sources(seeded).expect_err("an outside path must be refused");
    assert!(error.contains("outside the public root"), "{error}");
}

#[test]
fn the_dependency_guard_refuses_an_empty_table_rather_than_passing_it() {
    // The failure this whole file exists for. An empty inspection set is a
    // broken guard, and treating it as a clean subject is how a check goes
    // green for years after it stopped working.
    let error = inspect_dependency_sources("[package]\nname = \"x\"\n")
        .expect_err("zero declarations must not read as clean");
    assert!(error.contains("inspected 0"), "{error}");
}

// ============================================================
// Guard 2 — synthetic catalog fixtures
// ============================================================

const CATALOG_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures/seam/catalog");

/// The one digest a synthetic fixture may carry: SHA-256 of the empty string.
///
/// The same value every other test in this crate uses as a stand-in. Pinning it
/// means a fixture cannot quietly acquire a digest that looks like it addresses
/// real content, which is the shape a plausible-real catalog entry takes.
const SYNTHETIC_DIGEST: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Words a fixture must not assert, because a fixture cannot know them.
///
/// Authorship and trust are claims about the world. A test corpus that carries
/// them teaches a reader that the corpus is evidence of something it never
/// observed, and the first time one is copied into a real catalog it arrives
/// pre-blessed.
const FORBIDDEN_CLAIMS: [&str; 6] = [
    "verified: true",
    "trusted: true",
    "signature:",
    "author:",
    "publisher:",
    "endorsed",
];

/// Refuse a catalog fixture that could be mistaken for a real entry.
fn inspect_catalog_entry(file: &str, text: &str) -> Result<(), String> {
    let name = text
        .lines()
        .find_map(|l| l.strip_prefix("name:"))
        .map(str::trim)
        .ok_or_else(|| format!("{file} declares no name"))?;
    if !name.starts_with("fixture-") {
        return Err(format!(
            "{file} declares `{name}`, which does not announce itself as a fixture"
        ));
    }

    for claim in FORBIDDEN_CLAIMS {
        if text.contains(claim) {
            return Err(format!(
                "{file} asserts `{claim}`, which a fixture cannot know"
            ));
        }
    }

    // Any host-shaped token must be unresolvable. `.invalid` is reserved by
    // RFC 2606 precisely so a test can name a host that can never exist.
    for token in text.split(|c: char| c.is_whitespace() || c == '"' || c == '\'') {
        let token = token.trim_end_matches(['.', ',', ')', ']']);
        if (token.starts_with("http://") || token.starts_with("https://"))
            && !token.trim_end_matches('/').ends_with(".invalid")
            && !token.contains(".invalid/")
        {
            return Err(format!("{file} names a resolvable origin: {token}"));
        }
    }

    // Digests, if present, are the one synthetic value.
    for line in text.lines() {
        if let Some((_, rest)) = line.split_once("sha256:") {
            let digest = rest.trim().trim_matches(['"', '\'']);
            if digest != SYNTHETIC_DIGEST {
                return Err(format!(
                    "{file} carries a digest that is not the synthetic one: {digest}"
                ));
            }
        }
    }

    Ok(())
}

fn catalog_files() -> Vec<(String, String)> {
    let dir = Path::new(CATALOG_DIR);
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| {
        panic!(
            "catalog fixture directory {} is unreadable: {e}",
            dir.display()
        )
    });
    let mut out = Vec::new();
    for entry in entries {
        let entry = entry.expect("directory entry");
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.ends_with(".yaml") {
            let text = std::fs::read_to_string(entry.path()).expect("fixture is readable");
            out.push((name, text));
        }
    }
    out.sort();
    out
}

#[test]
fn every_catalog_fixture_announces_itself_as_one() {
    let files = catalog_files();

    // The count first, and a floor rather than an equality so adding a fixture
    // does not require editing this number.
    assert!(
        files.len() >= 3,
        "expected at least 3 catalog fixtures, walked {}",
        files.len()
    );

    let mut checked = 0usize;
    for (name, text) in &files {
        inspect_catalog_entry(name, text).unwrap_or_else(|e| panic!("catalog fixture: {e}"));
        checked += 1;
    }
    assert_eq!(checked, files.len());
}

#[test]
fn the_catalog_guard_refuses_a_plausible_real_identity() {
    let error = inspect_catalog_entry("seed.yaml", "name: code-review\ndescription: d\n")
        .expect_err("an unprefixed identity must be refused");
    assert!(error.contains("does not announce itself"), "{error}");
}

#[test]
fn the_catalog_guard_refuses_a_resolvable_origin() {
    let error = inspect_catalog_entry(
        "seed.yaml",
        "name: fixture-a\norigin: https://registry.example.com/skills/a\n",
    )
    .expect_err("a resolvable origin must be refused");
    assert!(error.contains("resolvable origin"), "{error}");
}

#[test]
fn the_catalog_guard_refuses_an_authorship_or_trust_claim() {
    // Both halves, because they are different mistakes: one asserts who made
    // the thing, the other asserts that somebody checked it.
    let authored = inspect_catalog_entry("seed.yaml", "name: fixture-a\nauthor: someone\n")
        .expect_err("an authorship claim must be refused");
    assert!(authored.contains("cannot know"), "{authored}");

    let trusted = inspect_catalog_entry("seed.yaml", "name: fixture-a\nverified: true\n")
        .expect_err("a trust claim must be refused");
    assert!(trusted.contains("cannot know"), "{trusted}");
}

#[test]
fn the_catalog_guard_refuses_a_digest_that_addresses_something() {
    let error = inspect_catalog_entry(
        "seed.yaml",
        &format!("name: fixture-a\npin: sha256:{}\n", "b".repeat(64)),
    )
    .expect_err("a non-synthetic digest must be refused");
    assert!(error.contains("not the synthetic one"), "{error}");
}
