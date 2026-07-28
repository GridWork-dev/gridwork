#!/usr/bin/env bash
# check-claims.sh — canonical public claims must agree everywhere they appear.
# Copy drifts one file at a time; this gate makes a one-file edit red until
# every surface says the same thing.
set -euo pipefail

cd "$(dirname "$0")/.."
fail=0

need() { # need <file> <pattern> <claim-name>
  if ! grep -qiE -- "$2" "$1"; then
    echo "check-claims: '$3' missing from $1 (expected /$2/)" >&2
    fail=1
  fi
}

# C1 — MSRV: one number, four surfaces
msrv=$(grep -oE 'rust-version = "[0-9.]+"' Cargo.toml | grep -oE '[0-9.]+')
need README.md "MSRV ${msrv//./\\.}" "MSRV $msrv"
need CONTRIBUTING.md "MSRV \\(${msrv//./\\.}\\)" "MSRV $msrv"
need .github/workflows/ci.yml "toolchain: \"${msrv//./\\.}\"" "MSRV $msrv toolchain"

# C2 — the install command, future-tensed everywhere until the crate publishes
need README.md 'cargo install gridwork' "install command"
need ROADMAP.md 'cargo install gridwork' "install command"
need site/index.html 'cargo install gridwork' "install command"

# C3 — terminal-only surface ("web console" appears only inside its negation;
# README line-wraps the phrase, so match the noun and rely on presence)
need README.md 'web console' "terminal-only"
need ROADMAP.md 'No web console' "terminal-only"
need site/index.html 'no web console' "terminal-only"

# C4 — the current stage number agrees across README, site, and ROADMAP
readme_stage=$(grep -oE 'stage [0-9] of 5' README.md | grep -oE '^stage [0-9]' | grep -oE '[0-9]' | head -1)
site_stage=$(grep -oE 'stage [0-9]/5' site/index.html | grep -oE '[0-9]' | head -1)
roadmap_stage=$(grep -oE '^## [0-9] · .*\(current\)' ROADMAP.md | grep -oE '[0-9]' | head -1)
if [ -z "$readme_stage" ] || [ "$readme_stage" != "$site_stage" ] || [ "$readme_stage" != "$roadmap_stage" ]; then
  echo "check-claims: current-stage disagreement — README='$readme_stage' site='$site_stage' ROADMAP='$roadmap_stage'" >&2
  fail=1
fi

# C5 — the binary name claim
need README.md 'The binary is .gw.' "binary name"
need site/index.html '<code>gw</code>' "binary name"

# C6 — pre-1.0 support stance
need SECURITY.md 'no supported releases' "no supported releases"
need README.md "Don.t run .main." "main is not for use"

if [ "$fail" -ne 0 ]; then
  echo "check-claims: FAIL" >&2
  exit 1
fi
echo "check-claims: clean"
