#!/usr/bin/env bash
# check-theme-sync.sh — the site's CSS custom properties and the gwk-theme
# crate must agree, both directions:
#   1. every crate token (snake_case) exists in site/app/globals.css as
#      --kebab-case:<value>
#   2. every solid #RRGGBB custom property in the site exists in the crate
#   3. every SIGNAL_LIGHT value appears in BOTH alternate-theme blocks
# The crate is the source of truth; a mismatch is a design decision that
# hasn't been made in both places yet.
#
# Check 3 is not redundant with check 1. The light palette is declared twice —
# once under `@media (prefers-color-scheme: light)` for the OS setting and once
# under `:root[data-theme="light"]` for the explicit toggle — and checks 1 and 2
# are satisfied by EITHER copy. Delete one block entirely and they both stay
# green while half the readers get the wrong palette: a reader who picks light
# on a dark laptop, or a reader on a light laptop before the theme script runs,
# depending which block went. Verified by deleting each in turn.
set -euo pipefail

cd "$(dirname "$0")/.."

crate=crates/gwk-theme/src/lib.rs
site=site/app/globals.css
fail=0

# crate -> site
while IFS='|' read -r name value; do
  kebab=${name//_/-}
  if ! grep -Eqi -- "--${kebab}:[[:space:]]*${value}" "$site"; then
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
done < <(grep -Eo -- '--[a-z0-9-]*:[[:space:]]*#[0-9A-Fa-f]{3,6}' "$site" \
  | sed 's/--\([a-z0-9-]*\):[[:space:]]*\(#[0-9A-Fa-f]*\)/\1|\2/')

# Both alternate-theme blocks carry the whole light palette.
#
# The blocks are located by their opening selector and read to the first line
# that closes at column 0 — `@media` nests, so its own closing brace is the
# second one. Extracting by name rather than by line number means moving a block
# does not silently empty the check.
extract_block() {
  awk -v start="$1" -v depth="$2" '
    index($0, start) == 1 { inblock = 1 }
    inblock { print; n += gsub(/\{/, "{"); n -= gsub(/\}/, "}"); if (n == 0 && depth) { exit } }
  ' "$site"
}

media_block="$(extract_block '@media (prefers-color-scheme: light)' 1)"
attr_block="$(extract_block ':root[data-theme="light"]' 1)"

light_count=0
while IFS='|' read -r name value; do
  [ -n "$name" ] || continue
  light_count=$((light_count + 1))
  kebab=${name//_/-}
  for pair in "media:$media_block" "data-theme:$attr_block"; do
    label=${pair%%:*}
    body=${pair#*:}
    if ! printf '%s' "$body" | grep -Eqi -- "--${kebab}:[[:space:]]*${value}"; then
      echo "theme-sync: light token '${name}' (${value}) missing from the ${label} block" >&2
      fail=1
    fi
  done
done < <(grep -o 'LightToken { name: "[a-z0-9_]*", value: "#[0-9A-Fa-f]*"' "$crate" \
  | sed 's/LightToken { name: "\([a-z0-9_]*\)", value: "\(#[0-9A-Fa-f]*\)"/\1|\2/')

# A fold cannot tell "all fifteen matched" from "the grep found nothing to
# match". If the crate table is ever renamed out from under that pattern, the
# loop above runs zero times and reports clean.
if [ "$light_count" -eq 0 ]; then
  echo "theme-sync: read 0 light tokens from ${crate} — the alternate-theme check ran on nothing" >&2
  fail=1
fi

if [ "$fail" -ne 0 ]; then
  echo "theme-sync: FAIL" >&2
  exit 1
fi
echo "theme-sync: clean (${light_count} light tokens in both alternate blocks)"
