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

exec "$OXFMT" "$@" site contracts
