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

# House tooling assumes the conventional layout, where a repo root is itself a Bun project
# and its binaries sit in <repo>/node_modules/.bin. This repo has no root package.json —
# three independent projects share the checkout (.gridwork/project.toml) and oxlint is
# pinned once, under site/. Link the conventional path at the real binary so shared tooling
# resolves it without each copy being edited to look somewhere else, which is the fork D3
# exists to prevent. `/node_modules/` is gitignored at the root for exactly this.
mkdir -p node_modules/.bin
ln -sfn ../../site/node_modules/.bin/oxlint node_modules/.bin/oxlint

# `--report-unused-disable-directives` is what keeps the two `oxlint-disable-next-line`
# directives in site/app/page.tsx honest. Both suppress jsx-a11y/no-noninteractive-tabindex
# on a scrollable region, which HAS to be focusable for keyboard users (WCAG 2.1.1) — a
# thing the rule cannot see. That is a legitimate suppression today and dead weight the
# moment the markup changes, and a dead suppression is indistinguishable from a live one
# by reading. This flag makes the linter tell us instead. It is a CLI flag rather than a
# config key on purpose: the base config is vendored and byte-pinned, so tightening it here
# is a repo-local choice, not a fork of the house floor.
exec "$OXLINT" --report-unused-disable-directives "$@"
