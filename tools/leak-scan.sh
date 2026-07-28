#!/usr/bin/env bash
# Leak gate: private-estate identifiers must never appear in this public repo.
# Public patterns are GENERIC shapes (absolute home paths, CGNAT ranges, credential
# assignments, private-key blocks, session URLs) — the gate must not itself disclose
# any exact private value. Estate-specific patterns load from an UNTRACKED local file
# (tools/leak-scan.local, gitignored) so they never enter public history.
# CI proves the gate can fail by feeding it seeded violations.
# Scans TRACKED files only — that is what can actually publish; gitignored local
# session files legitimately hold private paths and must not redden the gate.
set -euo pipefail

patterns='/home/[a-z0-9_-]+/|/Users/[a-z0-9_-]+/|100\.(6[4-9]|[7-9][0-9]|1[01][0-9]|12[0-7])\.[0-9]+|_(TOKEN|SECRET|API_KEY|PASSWORD)=[^[:space:]]|BEGIN [A-Z ]*PRIVATE KEY|claude\.ai/code/session_'

here="$(cd "$(dirname "$0")" && pwd)"
if [[ -f "$here/leak-scan.local" ]]; then
  patterns="$patterns|$(grep -Ev '^[[:space:]]*(#|$)' "$here/leak-scan.local" | paste -sd'|' -)"
fi

if [[ "${1:-}" == "--stdin" ]]; then
  # Branch on grep's status explicitly: 0 = match (leak), 1 = clean. Anything else
  # (bad pattern, read error) must be a hard error — a negated grep would turn
  # status 2 into a silent pass, and -a treats every byte as text so a stray NUL
  # cannot demote the stream to "binary, skipped".
  status=0
  grep -Eaq "$patterns" || status=$?
  case "$status" in
    0) echo 'leak-scan: private identifiers found on stdin' >&2; exit 1 ;;
    1) exit 0 ;;
    *) echo "leak-scan: grep failed (status $status) — refusing to pass" >&2; exit "$status" ;;
  esac
fi

cd "$here/.."
# Defence-in-depth: flag binary-looking tracked files early with a clear message.
# file(1) classifies from the head of a file, so this guard alone is bypassable by
# bytes placed past its window — the pattern scan below therefore uses grep -a
# (every byte treated as text) and does not depend on this check for coverage.
# Key on ENCODING, not mime family — file(1) types scripts as
# application/javascript etc.; what matters is that the bytes are greppable text.
# Empty files report encoding "binary" but are trivially safe.
# One reviewed binary asset is exempt: site/og.png (the share card — tool-generated
# from public copy + the public palette, never hand-edited).
binaries=$(git ls-files -z ':!site/og.png' | xargs -0 -r file --mime-encoding \
  | grep -Ev ': *(us-ascii|utf-8|ascii)$' \
  | while IFS=: read -r f _; do [[ -s "$f" ]] && echo "$f: non-text encoding"; done || true)
if [[ -n "$binaries" ]]; then
  echo "$binaries"
  echo 'leak-scan: binary tracked files are unscannable — use text-safe encodings' >&2
  exit 1
fi
# The exemption above is keyed on path; pin it to its shape so arbitrary future
# bytes at that path cannot ride the carve-out.
if [[ "$(file -b --mime-type site/og.png)" != "image/png" ]]; then
  echo 'leak-scan: site/og.png is not image/png — the binary exemption covers only the reviewed PNG' >&2
  exit 1
fi
# Branch on output, not exit status: xargs exits 123 if any batch matches, and a real
# match in one batch plus a clean batch must still fail the gate loudly.
# grep -a, never -I: -I's binary sniff runs over the whole stream, so a NUL past
# file(1)'s window would silently skip the file. Noisy matches from binary bytes
# are the correct failure direction for a publication gate.
matches=$(git ls-files -z ':!tools/leak-scan.sh' | xargs -0 -r grep -Ean "$patterns" || true)
if [[ -n "$matches" ]]; then
  echo "$matches"
  echo 'leak-scan: private identifiers found' >&2
  exit 1
fi
echo 'leak-scan: clean'
