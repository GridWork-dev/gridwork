#!/usr/bin/env bash
# format.sh — the one invocation home for oxfmt in this repo (SPEC house-standards D2c).
#
# WHY A SCRIPT AND NOT A package.json SCRIPT. oxfmt reads its config and its ignore files
# from the CURRENT DIRECTORY ONLY; it does not walk up (`--disable-nested-config` only
# turns off the search DOWNWARD into subdirectories). `.oxfmtrc.json` and `.prettierignore`
# live at the repo root, so the formatter has exactly one correct cwd. Run from `site/`,
# oxfmt finds no config, falls back to upstream defaults, and `--check` goes green having
# verified a different set of rules than the one this repo pinned. There is also no root
# package.json to hang a script on: three independent projects share this checkout
# (.gridwork/project.toml), and a fourth one existing only to hold two scripts is a fourth
# lockfile to keep current.
#
# WHY THE TREES ARE NAMED POSITIONALLY. This repo is mostly Rust, and oxfmt formats TOML
# and YAML as well as JS/TS/CSS/JSON. crates/, xtask/, Cargo.toml, deny.toml, schema/ and
# .github/ are owned by rustfmt, cargo-deny and CI. A blacklist would have to stay ahead of
# every new root file, and it already missed deny.toml — which oxfmt duly offered to
# rewrite. A whitelist of the two Bun projects cannot miss. The exclusions that remain in
# .prettierignore are the ones INSIDE those two trees.
#
# Usage: tools/format.sh --check    (gate)
#        tools/format.sh            (write, oxfmt's default)
set -euo pipefail
cd "$(dirname "$0")/.."

OXFMT=./site/node_modules/.bin/oxfmt
if [ ! -x "$OXFMT" ]; then
  echo "format: $OXFMT missing — run 'bun install' in site/ first" >&2
  exit 2
fi

TREES=(site contracts)

# The trees are named positionally, which is the kind of argument a rename breaks in
# silence: oxfmt does not fail on a pattern that matches nothing, so a moved tree just
# stops being formatted. Assert they are there rather than inferring it from a count —
# dropping `contracts` costs 3 of 25 files, which no honest floor would catch.
for tree in "${TREES[@]}"; do
  if [ ! -d "$tree" ]; then
    echo "format: '$tree' does not exist — the formatter is aimed at a tree that moved" >&2
    exit 1
  fi
done

# HOW MANY FILES DID IT ACTUALLY READ. `oxfmt --check` over a path matching nothing prints
# "0 files" and exits 0 — measured, not assumed. So a green `--check` says either
# "everything is formatted" or "the formatter was pointed at nothing", and the exit code
# cannot tell them apart. This catches the other half: both trees present but emptied of
# formattable files by a widened ignore list. The floor sits far below today's 25 because
# the failure being guarded lands near zero.
#
# Not `exec`, therefore: the output has to be read before it is passed on. It is re-emitted
# whole, and the real exit code is preserved.
FMT_FLOOR="${FMT_FLOOR:-15}"
out="$("$OXFMT" "$@" "${TREES[@]}" 2>&1)" && rc=0 || rc=$?
printf '%s\n' "$out"

read_files="$(printf '%s' "$out" | grep -o 'on [0-9]* files' | grep -o '[0-9]*' || true)"
if [ "${read_files:-0}" -lt "$FMT_FLOOR" ]; then
  echo "format: only ${read_files:-0} files reached oxfmt — it is checking nothing" >&2
  exit 1
fi

exit "$rc"
