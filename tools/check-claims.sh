#!/usr/bin/env bash
# check-claims.sh — canonical public claims must agree everywhere they appear.
# Copy drifts one file at a time; this gate makes a one-file edit red until
# every surface says the same thing.
set -euo pipefail

cd "$(dirname "$0")/.."
fail=0
landing=site/app/page.tsx

need() { # need <file> <pattern> <claim-name>
  if ! grep -qiE -- "$2" "$1"; then
    echo "check-claims: '$3' missing from $1 (expected /$2/)" >&2
    fail=1
  fi
}

# Every extraction below ends in `|| true`. Under `set -e` a bare `v=$(grep … )`
# that matches nothing kills the script AT THE ASSIGNMENT — exit 1, no output, no
# "FAIL" line — which reads like a crash rather than a claim that went missing.
# Non-fatal capture lets the emptiness checks below actually run and say so.

# C1 — MSRV: one number, five surfaces
msrv=$(grep -oE 'rust-version = "[0-9.]+"' Cargo.toml | grep -oE '[0-9.]+' || true)
if [ -z "$msrv" ]; then
  # Guard, not paranoia: an empty msrv would degrade every pattern below to a bare
  # "MSRV " prefix that matches any version — the gate would pass while drifting.
  echo "check-claims: no rust-version in Cargo.toml — cannot pin MSRV" >&2
  fail=1
else
  need README.md "MSRV ${msrv//./\\.}" "MSRV $msrv"
  need CONTRIBUTING.md "MSRV \\(${msrv//./\\.}\\)" "MSRV $msrv"
  need .github/workflows/ci.yml "toolchain: \"${msrv//./\\.}\"" "MSRV $msrv toolchain"
  # The landing source states it twice (build line, crate-table caption). It was omitted here
  # and sat six minor versions behind unnoticed (1.88 against 1.94); `need` is
  # case-insensitive, so one pattern covers both spellings.
  need "$landing" "msrv ${msrv//./\\.}" "MSRV $msrv"
fi

# C2 — the install command, future-tensed everywhere until the crate publishes
need README.md 'cargo install gridwork' "install command"
need ROADMAP.md 'cargo install gridwork' "install command"
need "$landing" 'cargo install gridwork' "install command"

# C3 — terminal-only surface ("web console" appears only inside its negation;
# README line-wraps the phrase, so match the noun and rely on presence)
need README.md 'web console' "terminal-only"
need ROADMAP.md 'No web console' "terminal-only"
need "$landing" 'no web console' "terminal-only"

# C4 — the CURRENT stage number agrees across README, the landing source nav chip, ROADMAP,
# and the threat model. docs/contract/NAMING.md is deliberately NOT compared here:
# its "stage 1 of 5" says when the naming policy froze, so it is a constant, not a
# marker. Comparing it made every stage bump red until someone renumbered a
# sentence that must never move — C7 pins it to the constant instead.
readme_stage=$(grep -oE 'stage [0-9] of 5' README.md | grep -oE '^stage [0-9]' | grep -oE '[0-9]' | head -1 || true)
site_stage=$(grep -oE 'stage [0-9]/5' "$landing" | grep -oE '[0-9]' | head -1 || true)
roadmap_stage=$(grep -oE '^## [0-9] · .*\(current\)' ROADMAP.md | grep -oE '[0-9]' | head -1 || true)
threat_stage=$(grep -oE 'stage [0-9] of 5' docs/security/THREAT_MODEL.md | grep -oE '^stage [0-9]' | grep -oE '[0-9]' | head -1 || true)
if [ -z "$readme_stage" ] \
  || [ "$readme_stage" != "$site_stage" ] \
  || [ "$readme_stage" != "$roadmap_stage" ] \
  || [ "$readme_stage" != "$threat_stage" ]; then
  echo "check-claims: current-stage disagreement — README='$readme_stage' site='$site_stage' ROADMAP='$roadmap_stage' THREAT_MODEL='$threat_stage'" >&2
  fail=1
fi
# The landing source states the stage TWICE — the nav chip ("stage N/5", compared above) and
# the roadmap gauge prose ("stage N of 5", which the chip pattern cannot see). Pin
# the prose too; otherwise a bump that misses it ships a self-contradicting page
# with CI green.
if [ -n "$readme_stage" ]; then
  need "$landing" "stage ${readme_stage} of 5" "site roadmap-gauge stage"
fi

# C5 — the binary name claim
need README.md 'The binary is .gw.' "binary name"
need "$landing" '<code>gw</code>' "binary name"

# C6 — pre-1.0 support stance
need SECURITY.md 'no supported releases' "no supported releases"
need README.md "Don.t run .main." "main is not for use"

# C7 — the contract-freeze stage is HISTORICAL and permanent: the Contract stage is
# stage 1 forever. Pinned to the literal so a future current-stage bump renumbering
# it goes red — which dropping it from C4 alone would not catch.
need docs/contract/NAMING.md 'Frozen at the Contract stage \(stage 1 of 5\)' "contract-freeze stage"

if [ "$fail" -ne 0 ]; then
  echo "check-claims: FAIL" >&2
  exit 1
fi
echo "check-claims: clean"
