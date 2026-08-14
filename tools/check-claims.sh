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

# C4 — the CURRENT stage number agrees across every public surface that states it.
# docs/contract/NAMING.md and its site mirror are deliberately NOT compared here:
# its "stage 1 of 5" says when the naming policy froze, so it is a constant, not a
# marker. Comparing it made every stage bump red until someone renumbered a
# sentence that must never move — C7 pins it to the constant instead.
readme_stage=$(grep -oE 'stage [0-9] of 6' README.md | grep -oE '^stage [0-9]' | grep -oE '[0-9]' | head -1 || true)
site_stage=$(grep -oE 'stage [0-9]/6' "$landing" | grep -oE '[0-9]' | head -1 || true)
roadmap_stage=$(grep -oE '^## [0-9] · .*\(current\)' ROADMAP.md | grep -oE '[0-9]' | head -1 || true)
threat_stage=$(grep -oE 'stage [0-9] of 6' docs/security/THREAT_MODEL.md | grep -oE '^stage [0-9]' | grep -oE '[0-9]' | head -1 || true)
quickstart_stage=$(grep -oE 'stage [0-9] of 6' site/content/docs/quickstart.mdx | grep -oE '^stage [0-9]' | grep -oE '[0-9]' | head -1 || true)
site_security_stage=$(grep -oE 'stage [0-9] of 6' site/content/docs/security/index.mdx | grep -oE '^stage [0-9]' | grep -oE '[0-9]' | head -1 || true)
site_prose_stage=$(grep -oE 'stage [0-9] of 6' "$landing" | grep -oE '^stage [0-9]' | grep -oE '[0-9]' | head -1 || true)
# CONTRIBUTING.md joined at the 6' close: it sat a full stage behind for five
# days precisely because nothing compared it.
contributing_stage=$(grep -oE 'stage [0-9] of 6' CONTRIBUTING.md | grep -oE '^stage [0-9]' | grep -oE '[0-9]' | head -1 || true)
if [ -z "$readme_stage" ] \
  || [ "$readme_stage" != "$site_stage" ] \
  || [ "$readme_stage" != "$roadmap_stage" ] \
  || [ "$readme_stage" != "$threat_stage" ] \
  || [ "$readme_stage" != "$quickstart_stage" ] \
  || [ "$readme_stage" != "$site_security_stage" ] \
  || [ "$readme_stage" != "$site_prose_stage" ] \
  || [ "$readme_stage" != "$contributing_stage" ]; then
  echo "check-claims: current-stage disagreement — README='$readme_stage' site-chip='$site_stage' ROADMAP='$roadmap_stage' THREAT_MODEL='$threat_stage' quickstart='$quickstart_stage' site-security='$site_security_stage' site-prose='$site_prose_stage' CONTRIBUTING='$contributing_stage'" >&2
  fail=1
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
need site/content/docs/contract/index.mdx 'Frozen at the Contract stage \(stage 1 of 5\)' "site contract-freeze stage"
# The parity promise is the same shape: "stage 3" there records WHEN the roadmap
# promised the matrix, not where the ladder stands. It was the one public stage
# string neither compared by C4 nor pinned here (2026-08-04 review triage) — a
# renumbering sweep could move it silently. Pin canonical + mirror to the constant.
need docs/PARITY.md 'for stage 3, defined before any adapter exists' "parity-promise stage"
need site/content/docs/parity.mdx 'for stage 3, defined before any adapter exists' "site parity-promise stage"

# C8 — the crates table names the same crate SET on both surfaces (README table,
# landing-page array). Name-set only: wording and status drift are readable on
# review, a crate present on one surface and absent from the other is not — that
# is exactly how gwk-pty-host shipped in the README and never reached the site.
readme_crates=$(grep -E '^\| \[?`' README.md | sed -E 's/^\| \[?`([^`]+)`.*/\1/' | LC_ALL=C sort || true)
site_crates=$(sed -n '/^const crates = \[/,/^\] as const;/p' "$landing" | tr -d '\n ' | grep -oE '\["[^"]+"' | tr -d '["' | LC_ALL=C sort || true)
if [ -z "$readme_crates" ] || [ "$readme_crates" != "$site_crates" ]; then
  echo "check-claims: crates-table disagreement — README=[$(echo "$readme_crates" | tr '\n' ' ')] site=[$(echo "$site_crates" | tr '\n' ' ')]" >&2
  fail=1
fi

# C9 — how many jobs `verify` gathers: one number, three surfaces, derived.
# CLAUDE.md tells contributors that when a check aggregates, assert the count
# first. Nothing applied that to the aggregate count itself: three files state
# it in prose and none read ci.yml, so renovate.json sat at 17 while the
# workflow had grown to 18. Neither number is checkable by reading the file that
# claims it, which is why this derives both from the workflow.
counts=$(awk '
  /^jobs:/                      { in_jobs = 1; next }
  !in_jobs                      { next }
  /^  [A-Za-z0-9_-]+:/          { jobs++; is_verify = ($1 == "verify:"); in_needs = 0 }
  is_verify && /^    needs:/    { in_needs = 1; next }
  is_verify && /^    [A-Za-z]/  { in_needs = 0 }
  in_needs && /^      - /       { needs++ }
  END                           { print jobs + 0, needs + 0 }
' .github/workflows/ci.yml)
ci_jobs=${counts% *}
verify_needs=${counts#* }
# Count first, verdict second. An awk that matched nothing prints "0 0", and
# every `need` below would then search for "aggregates 0" and fail with a
# confusing message about prose rather than the real fault, which is that this
# gate lost its grip on the workflow.
if [ "$ci_jobs" -lt 2 ] || [ "$verify_needs" -lt 1 ]; then
  echo "check-claims: could not read job counts from ci.yml (jobs=$ci_jobs needs=$verify_needs)" >&2
  fail=1
else
  # `verify` is one of the job keys; the prose counts the others against it.
  others=$((ci_jobs - 1))
  need renovate.json "aggregates $verify_needs jobs" "verify aggregate count"
  # Both of these line-wrap after the number, so the total is matched separately.
  need CLAUDE.md "aggregates $verify_needs of the" "verify aggregate count"
  need .gridwork/project.toml "aggregates $verify_needs" "verify aggregate count"
  need CLAUDE.md "$others jobs in" "ci job total"
  need .gridwork/project.toml "$others jobs plus" "ci job total"
fi

if [ "$fail" -ne 0 ]; then
  echo "check-claims: FAIL" >&2
  exit 1
fi
echo "check-claims: clean"
