#!/usr/bin/env bash
# verify-gates.sh — the assertion behind ci.yml's `verify` aggregator.
#
# Reads GitHub's `toJSON(needs)` on stdin and exits non-zero unless every
# dependency finished `success`. It lives here rather than inline in the
# workflow for one reason: the seeded-violation step must run THE SAME CODE
# the real step runs. A self-test that re-types the assertion proves the copy
# in the self-test works, which is not the claim anyone wants.
#
# Why an aggregator exists at all: branch protection listed 15 contexts against
# 20 jobs, and the two it missed were `pty-host` — a crate also outside
# `default-members`, so nothing else covered it either — and `perf`, the rollup
# the whole attempt/retry machinery feeds. A hand-curated list in a settings
# page drifts silently because nothing compares it to the workflow. One
# required context, computed from `needs:`, cannot.
set -euo pipefail

# Bump deliberately when a gate joins or leaves `verify`'s `needs:` list. This
# is the guard against the failure this whole gate exists to prevent, one level
# up: deleting a `needs:` entry would otherwise shrink coverage silently, and
# with only one required context there is no second place that would notice.
EXPECTED_GATES="${EXPECTED_GATES:-17}"

payload="$(cat)"

if ! printf '%s' "$payload" | jq -e . >/dev/null 2>&1; then
  echo "verify-gates: stdin is not valid JSON" >&2
  exit 2
fi

count="$(printf '%s' "$payload" | jq -r 'length')"

# `all()` over an empty object is TRUE. Without this, a `needs:` list that
# failed to expand — a typo, a refactor, a templating slip — would report every
# gate green having checked none of them, which is precisely the shape of
# failure a single required context makes invisible. The count decides, not the
# fold.
if [ "$count" -eq 0 ]; then
  echo "verify-gates: no dependencies reported — refusing to call that green" >&2
  exit 1
fi

if [ "$count" -lt "$EXPECTED_GATES" ]; then
  echo "verify-gates: ${count} dependencies reported, expected at least ${EXPECTED_GATES}" >&2
  echo "verify-gates: a gate left verify's needs: list without EXPECTED_GATES moving" >&2
  exit 1
fi

printf '%s' "$payload" | jq -r 'to_entries[] | "  \(.key): \(.value.result)"' | sort

not_green="$(printf '%s' "$payload" |
  jq -r '[to_entries[] | select(.value.result != "success") | .key] | join(", ")')"

if [ -n "$not_green" ]; then
  echo "verify-gates: not green — ${not_green}" >&2
  exit 1
fi

echo "verify-gates: ${count} gates green"
