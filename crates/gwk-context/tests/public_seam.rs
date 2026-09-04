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

/// The table kinds a dependency can be declared in. A walk that reads only
/// `[dependencies]` is blind to the other three, and the same declaration
/// moved one table down is invisible to it.
const DEPENDENCY_KINDS: &[&str] = &["dependencies", "dev-dependencies", "build-dependencies"];

/// Every dependency this crate names, from all four kinds of dependency table.
///
/// Parsed rather than line-scanned, for the reason the workspace reader above
/// was: a section header is not a reliable delimiter. The earlier revision set
/// its flag from `line == "[dependencies]"`, so every other table header turned
/// it off and the declarations under it were never seen at all.
///
/// Returns the count of tables walked alongside the names, so the caller can
/// tell a crate that declares little from a walk that read almost nothing.
fn own_dependency_names(manifest: &str) -> Result<(BTreeSet<String>, usize), String> {
    let parsed: toml::Table = manifest
        .parse()
        .map_err(|e| format!("crate manifest is not TOML: {e}"))?;
    let mut out: BTreeSet<String> = BTreeSet::new();
    let mut tables = 0usize;

    // The sub-table spelling (`[dependencies.foo]`) needs no special case: the
    // parser folds it into the same table, which is the point of parsing.
    let mut take = |table: &toml::Value| {
        if let Some(entries) = table.as_table() {
            tables += 1;
            for name in entries.keys() {
                out.insert(name.clone());
            }
        }
    };

    for kind in DEPENDENCY_KINDS {
        if let Some(table) = parsed.get(*kind) {
            take(table);
        }
    }

    // `[target.'cfg(...)'.dependencies]` and its dev and build siblings. A walk
    // that stops at the top level is defeated by one cfg expression.
    if let Some(targets) = parsed.get("target").and_then(toml::Value::as_table) {
        for entry in targets.values() {
            let Some(entry) = entry.as_table() else {
                continue;
            };
            for kind in DEPENDENCY_KINDS {
                if let Some(table) = entry.get(*kind) {
                    take(table);
                }
            }
        }
    }

    Ok((out, tables))
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
    let (names, tables) =
        own_dependency_names(&own).unwrap_or_else(|e| panic!("crate dependency seam: {e}"));

    // Both counts first, and they catch different regressions. An empty name
    // set is a broken parse. A table count of one is a walk that found
    // `[dependencies]` and stopped — the exact shape this reader used to have,
    // which reads clean because the names it did find are all legitimate.
    assert!(
        !names.is_empty(),
        "this crate declares no dependencies — the parse is broken, not the manifest"
    );
    assert!(
        tables >= 2,
        "expected at least 2 dependency tables in this crate's manifest, walked {tables}"
    );

    // And every one of them resolves to a declaration the guard above checked,
    // so a dependency added directly to this crate with its own git or path
    // source cannot slip past a guard that only reads the workspace table.
    //
    // That sentence is only true because the walk above reads all four table
    // kinds. While it read `[dependencies]` alone it was false in the other
    // three: the same declaration in `[dev-dependencies]`,
    // `[build-dependencies]`, or under a `[target.'cfg(..)']` block was never
    // inspected, so it could carry any source at all. This crate has a real
    // `[dev-dependencies]` table, so the claim was false about its own
    // manifest and not merely in principle.
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
fn the_crate_manifest_walk_reads_every_kind_of_dependency_table() {
    // One declaration per table kind, each carrying a source the workspace
    // table does not declare. Against the earlier line-based reader only `a`
    // was ever seen: `[dev-dependencies]`, `[build-dependencies]` and the
    // `[target.'cfg(..)']` block each turned its section flag off, so `b`,
    // `c` and `d` were not merely unchecked, they were invisible. Their names
    // never reached the cross-check, which is why nothing downstream could
    // notice — a name that is never extracted cannot fail a containment test.
    let seeded = "[package]\n\
                  name = \"x\"\n\n\
                  [dependencies]\n\
                  a = \"1\"\n\n\
                  [dev-dependencies]\n\
                  b = { path = \"../../elsewhere\" }\n\n\
                  [build-dependencies]\n\
                  c = { path = \"../../elsewhere\" }\n\n\
                  [target.'cfg(unix)'.dependencies]\n\
                  d = { git = \"https://example.invalid/d\" }\n";

    let (names, tables) = own_dependency_names(seeded).expect("the seeded manifest parses");
    assert_eq!(tables, 4, "every dependency table kind was walked");
    let expected: BTreeSet<String> = ["a", "b", "c", "d"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_eq!(
        names, expected,
        "a name declared in any table kind is a name the cross-check must see"
    );

    // The sub-table spelling folds into the same table rather than adding one,
    // so the count above stays honest when a dependency is written long-form.
    let sub = "[dependencies]\n\
               a = \"1\"\n\n\
               [dependencies.b]\n\
               path = \"../../elsewhere\"\n";
    let (sub_names, sub_tables) = own_dependency_names(sub).expect("the sub-table fixture parses");
    assert_eq!(sub_tables, 1);
    assert_eq!(
        sub_names,
        ["a", "b"]
            .into_iter()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>()
    );
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
