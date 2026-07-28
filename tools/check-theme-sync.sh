#!/usr/bin/env bash
# check-theme-sync.sh — the site's CSS custom properties and the gwk-theme
# crate must agree, both directions:
#   1. every crate token (snake_case) exists in site/index.html as
#      --kebab-case:<value>
#   2. every solid #RRGGBB custom property in the site exists in the crate
# The crate is the source of truth; a mismatch is a design decision that
# hasn't been made in both places yet.
set -euo pipefail

cd "$(dirname "$0")/.."

crate=crates/gwk-theme/src/lib.rs
site=site/index.html
fail=0

# crate -> site
while IFS='|' read -r name value; do
  kebab=${name//_/-}
  if ! grep -qi -- "--${kebab}:${value}" "$site"; then
    echo "theme-sync: crate token '${name}' (${value}) missing or different in ${site} (expected --${kebab}:${value})" >&2
    fail=1
  fi
done < <(grep -o 'name: "[a-z0-9_]*", value: "#[0-9A-Fa-f]*"' "$crate" \
  | sed 's/name: "\([a-z0-9_]*\)", value: "\(#[0-9A-Fa-f]*\)"/\1|\2/')

# site -> crate (solid hex custom properties only; derived rgba/shadow/font
# properties are site-local and exempt)
while IFS='|' read -r kebab value; do
  snake=${kebab//-/_}
  if ! grep -qi "name: \"${snake}\", value: \"${value}\"" "$crate"; then
    echo "theme-sync: site property --${kebab}:${value} has no crate token '${snake}'" >&2
    fail=1
  fi
done < <(grep -o -- '--[a-z0-9-]*:#[0-9A-Fa-f]\{6\}' "$site" \
  | sed 's/--\([a-z0-9-]*\):\(#[0-9A-Fa-f]*\)/\1|\2/')

if [ "$fail" -ne 0 ]; then
  echo "theme-sync: FAIL" >&2
  exit 1
fi
echo "theme-sync: clean"
