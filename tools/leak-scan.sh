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
  ! grep -Eq "$patterns"
  exit
fi

cd "$here/.."
# Reject binary-looking tracked files: grep -I silently skips them, which would blind
# the gate. Key on ENCODING, not mime family — file(1) types scripts as
# application/javascript etc.; what matters is that the bytes are greppable text.
# Empty files report encoding "binary" but are trivially safe.
binaries=$(git ls-files -z | xargs -0 -r file --mime-encoding \
  | grep -Ev ': *(us-ascii|utf-8|ascii)$' \
  | while IFS=: read -r f _; do [[ -s "$f" ]] && echo "$f: non-text encoding"; done || true)
if [[ -n "$binaries" ]]; then
  echo "$binaries"
  echo 'leak-scan: binary tracked files are unscannable — use text-safe encodings' >&2
  exit 1
fi
# Branch on output, not exit status: xargs exits 123 if any batch matches, and a real
# match in one batch plus a clean batch must still fail the gate loudly.
matches=$(git ls-files -z ':!tools/leak-scan.sh' | xargs -0 -r grep -EIn "$patterns" || true)
if [[ -n "$matches" ]]; then
  echo "$matches"
  echo 'leak-scan: private identifiers found' >&2
  exit 1
fi
echo 'leak-scan: clean'
