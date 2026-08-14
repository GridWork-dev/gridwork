#!/usr/bin/env bash
# lint.sh — the one invocation home for oxlint in this repo (SPEC house-standards D2b).
#
# Same cwd constraint as tools/format.sh: the root `.oxlintrc.json` and the nested
# `site/.oxlintrc.json` are discovered relative to where oxlint starts, so root is the only
# correct cwd. Unlike format, this walks the WHOLE tree rather than naming paths — an
# unlinted new directory is a silent coverage hole, whereas an accidentally formatted
# generated file is a loud red gate. Exclusions live in the root config's `ignorePatterns`.
#
# Usage: tools/lint.sh              (gate)
#        tools/lint.sh --fix        (safe autofixes only — see the warning below)
#
# NEVER pass --fix-suggestions. oxlint's *suggestions* are not equivalence-preserving:
# on `no-console` it deletes the statement outright rather than rewriting it.
set -euo pipefail
cd "$(dirname "$0")/.."

OXLINT=./site/node_modules/.bin/oxlint
if [ ! -x "$OXLINT" ]; then
  echo "lint: $OXLINT missing — run 'bun install' in site/ first" >&2
  exit 2
fi

# HOW MANY FILES DID IT ACTUALLY READ. A clean exit is not evidence: oxlint over an empty
# set exits 0 having linted nothing, and every way this gate rots — a widened
# `ignorePatterns`, a moved tree, a config that fails to resolve — arrives disguised as a
# green run. So the count is checked before the findings are, and the count decides.
#
# The probe is scoped to `.` regardless of the arguments below. The hole being guarded is
# the gate case, where nobody passes a path; a developer linting one file already knows
# what they aimed at.
#
# The floor is far below today's 20 rather than just under it: the failure being guarded
# lands at zero or a handful, so a tight floor buys nothing and reds on ordinary churn.
LINT_FLOOR="${LINT_FLOOR:-10}"
read_files="$("$OXLINT" --format=json . | grep -o '"number_of_files": *[0-9]*' |
  grep -o '[0-9]*$' || true)"
echo "lint: oxlint read ${read_files:-0} files"
if [ "${read_files:-0}" -lt "$LINT_FLOOR" ]; then
  echo "lint: only ${read_files:-0} files reached oxlint — it is linting nothing" >&2
  exit 1
fi

# `--report-unused-disable-directives` is what keeps the two `oxlint-disable-next-line`
# directives in site/app/page.tsx honest. Both suppress jsx-a11y/no-noninteractive-tabindex
# on a scrollable region, which HAS to be focusable for keyboard users (WCAG 2.1.1) — a
# thing the rule cannot see. That is a legitimate suppression today and dead weight the
# moment the markup changes, and a dead suppression is indistinguishable from a live one
# by reading. This flag makes the linter tell us instead. It is a CLI flag rather than a
# config key on purpose: the base config is vendored and byte-pinned, so tightening it here
# is a repo-local choice, not a fork of the house floor.
exec "$OXLINT" --report-unused-disable-directives "$@"
