//! Stamp the build with the public revision it was built from — or with nothing.
//!
//! Genesis records the revision the kernel was initialized at, and `gw kernel
//! status` reports the revision the running daemon was built from, so an
//! operator can compare the two. That comparison is only worth anything if an
//! unstamped build says so rather than inventing a value: a placeholder would
//! make two different builds compare equal.
//!
//! Three sources, in order:
//!
//! 1. `GWK_PUBLIC_REVISION` in the build environment — what a release sets.
//! 2. `git rev-parse HEAD`, but only with a CLEAN working tree. A dirty tree's
//!    HEAD does not describe the bytes being compiled, and stamping it would be
//!    the same lie as a placeholder in a longer form.
//! 3. Nothing. The constant becomes `None`, `gw build-info` reports it absent,
//!    and a daemon refuses to start — which is `Daemon::new`'s existing rule,
//!    not a new one.

use std::process::Command;

fn main() {
    println!("cargo::rerun-if-env-changed=GWK_PUBLIC_REVISION");
    if let Some(revision) = from_env().or_else(from_clean_git) {
        println!("cargo::rustc-env=GW_PUBLIC_REVISION={revision}");
    }
}

fn from_env() -> Option<String> {
    std::env::var("GWK_PUBLIC_REVISION")
        .ok()
        .filter(|value| is_revision(value))
}

fn from_clean_git() -> Option<String> {
    // Rebuild when HEAD moves, so `build-info` cannot report the commit before
    // last. Asked of git rather than assumed, because a worktree's git
    // directory is not `.git` in the package root.
    if let Some(dir) = git(&["rev-parse", "--absolute-git-dir"]) {
        println!("cargo::rerun-if-changed={dir}/HEAD");
    }
    // `--porcelain` prints one line per difference and nothing at all for a
    // clean tree, which is exactly the test.
    if !git(&["status", "--porcelain"])?.is_empty() {
        return None;
    }
    git(&["rev-parse", "HEAD"]).filter(|value| is_revision(value))
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8(out.stdout).ok()?.trim().to_owned())
}

/// The same shape the kernel requires of a recorded revision: 40 lowercase hex.
fn is_revision(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
