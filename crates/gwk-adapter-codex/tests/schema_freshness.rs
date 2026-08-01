//! Re-runs `codex app-server generate-json-schema` and diffs its output
//! against `schemas/` — the same generated-artifact-freshness discipline
//! the rest of this repo applies to other machine-generated files.
//!
//! Skips with a printed message (not a failure) when no `codex` binary is
//! on `PATH`, or when the installed one reports a version other than the
//! pin this crate vendored against: `docs/PARITY.md` is explicit that the
//! matrix — and by extension this check — "runs locally, never in public
//! CI", because public CI "must never acquire an engine binary, an engine
//! login, or a network path to either". A `codex` at the pinned version
//! turns this from a skip into a real verification.

use std::path::{Path, PathBuf};
use std::process::Command;

/// `docs/PARITY.md`'s version-pins table: "Codex | `codex-cli 0.146.0`".
/// Vendored against exactly this string; a different installed version is
/// treated the same as no binary at all, because diffing against it would
/// not prove anything about what was actually vendored.
const PINNED_CODEX_VERSION: &str = "codex-cli 0.146.0";

/// Every file this crate vendored under `schemas/`, relative to that
/// directory — kept in sync with `schemas/PROVENANCE.md`'s table and
/// `crates/gwk-adapter-codex/schemas/{,v2/}*.json` in one place. A file
/// added to one without the other is exactly the drift this test exists to
/// catch, so the count below is asserted against the directory listing.
const VENDORED_FILES: &[&str] = &[
    "JSONRPCMessage.json",
    "ServerNotification.json",
    "ServerRequest.json",
    "CommandExecutionRequestApprovalParams.json",
    "CommandExecutionRequestApprovalResponse.json",
    "FileChangeRequestApprovalParams.json",
    "FileChangeRequestApprovalResponse.json",
    "v2/ThreadStartedNotification.json",
    "v2/ThreadStatusChangedNotification.json",
    "v2/ThreadClosedNotification.json",
    "v2/TurnCompletedNotification.json",
    "v2/ErrorNotification.json",
    "v2/ItemStartedNotification.json",
    "v2/ItemCompletedNotification.json",
    "v2/ThreadTokenUsageUpdatedNotification.json",
    "v2/ServerRequestResolvedNotification.json",
];

fn vendored_schemas_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("schemas")
}

fn installed_codex_version() -> Option<String> {
    let output = Command::new("codex").arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

#[test]
fn every_vendored_file_is_listed_and_exists() {
    // Independent of a live `codex` binary: this just proves the const
    // list above and the directory on disk agree, so the drift the main
    // check below cares about (vendored-vs-freshly-generated) is not
    // masked by a stale const list silently skipping a file.
    let dir = vendored_schemas_dir();
    for relative in VENDORED_FILES {
        let path = dir.join(relative);
        assert!(
            path.is_file(),
            "{relative} is listed in VENDORED_FILES but missing on disk at {}",
            path.display()
        );
    }
    let on_disk = count_json_files(&dir);
    assert_eq!(
        on_disk,
        VENDORED_FILES.len(),
        "schemas/ holds {on_disk} *.json files but VENDORED_FILES lists {} — \
         a file was added or removed on one side without the other",
        VENDORED_FILES.len()
    );
}

fn count_json_files(dir: &Path) -> usize {
    let mut count = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "json") {
                count += 1;
            }
        }
    }
    count
}

#[test]
fn vendored_schemas_match_a_fresh_generation() {
    let Some(installed) = installed_codex_version() else {
        eprintln!(
            "schema_freshness: SKIPPED — no `codex` binary on PATH. \
             This is expected in public CI (docs/PARITY.md); install \
             codex-cli to turn this skip into a real verification."
        );
        return;
    };
    if installed != PINNED_CODEX_VERSION {
        eprintln!(
            "schema_freshness: SKIPPED — installed `codex` reports \
             {installed:?}, vendored against {PINNED_CODEX_VERSION:?}. \
             Diffing against a different version would not verify what \
             was actually vendored."
        );
        return;
    }

    let out_dir = std::env::temp_dir().join(format!(
        "gwk-adapter-codex-schema-freshness-{}",
        std::process::id()
    ));
    // Best-effort: a leftover directory from a prior crashed run should not
    // fail this run, and this run's own cleanup below is likewise
    // best-effort — a stray temp directory is a nuisance, not a test bug.
    let _ = std::fs::remove_dir_all(&out_dir);
    std::fs::create_dir_all(&out_dir).expect("creating a fresh temp output directory");

    // Derivation: CODEX-APP-SERVER app-server README — "you can dump ... a
    // JSON Schema bundle via `codex app-server generate-json-schema`", and
    // "each output is specific to the version of Codex you used to run the
    // command", which is why this test asserts the installed version above
    // rather than diffing against whatever happens to be on PATH.
    let status = Command::new("codex")
        .args(["app-server", "generate-json-schema", "--out"])
        .arg(&out_dir)
        .arg("--experimental")
        .status()
        .expect("running codex app-server generate-json-schema");
    assert!(
        status.success(),
        "codex app-server generate-json-schema exited {status}"
    );

    let vendored_dir = vendored_schemas_dir();
    let mut mismatches = Vec::new();
    for relative in VENDORED_FILES {
        let vendored_path = vendored_dir.join(relative);
        let fresh_path = out_dir.join(relative);

        let vendored_text = std::fs::read_to_string(&vendored_path)
            .unwrap_or_else(|e| panic!("reading vendored {relative}: {e}"));
        let Ok(fresh_text) = std::fs::read_to_string(&fresh_path) else {
            mismatches.push(format!(
                "{relative}: the fresh generation did not produce this file at {}",
                fresh_path.display()
            ));
            continue;
        };

        // Parsed, not byte-compared: this test's job is to catch a real
        // schema change, not to enforce the generator's own whitespace —
        // if `codex` ever reformats its own output without changing a
        // single definition, that is not drift this crate needs to know
        // about.
        let vendored_value: serde_json::Value = serde_json::from_str(&vendored_text)
            .unwrap_or_else(|e| panic!("parsing vendored {relative} as JSON: {e}"));
        let fresh_value: serde_json::Value = serde_json::from_str(&fresh_text)
            .unwrap_or_else(|e| panic!("parsing freshly generated {relative} as JSON: {e}"));

        if vendored_value != fresh_value {
            mismatches.push(format!(
                "{relative}: vendored copy differs from a fresh generation at the pinned version"
            ));
        }
    }

    let _ = std::fs::remove_dir_all(&out_dir);

    assert!(
        mismatches.is_empty(),
        "vendored schemas are stale — re-run `codex app-server generate-json-schema \
         --out <dir> --experimental` at {PINNED_CODEX_VERSION}, copy the changed files \
         into schemas/, and update schemas/PROVENANCE.md if the set of derived-from \
         fields changed:\n{}",
        mismatches.join("\n")
    );
}
