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

base_patterns='/home/[a-z0-9_-]+/|/Users/[a-z0-9_-]+/|100\.(6[4-9]|[7-9][0-9]|1[01][0-9]|12[0-7])\.[0-9]+|_(TOKEN|SECRET|API_KEY|PASSWORD)=[^[:space:]]|BEGIN [A-Z ]*PRIVATE KEY|claude\.ai/code/session_'

here="$(cd "$(dirname "$0")" && pwd)"
overlay_patterns=''
if [[ -f "$here/leak-scan.local" ]]; then
  # An all-comment/blank local file must yield NO overlay tier, not an empty ERE
  # (an empty alternation branch matches every line and would redden the whole tree).
  overlay_patterns="$(grep -Ev '^[[:space:]]*(#|$)' "$here/leak-scan.local" | paste -sd'|' - || true)"
fi
patterns="$base_patterns"
if [[ -n "$overlay_patterns" ]]; then
  patterns="$patterns|$overlay_patterns"
fi

# Validate the assembled pattern set once, before any scan — a malformed ERE (most
# likely a bad line in the untracked leak-scan.local, exactly where estate patterns
# live) makes every grep below exit 2 with empty stdout, which would read as "clean"
# and fail the gate OPEN. An empty stream never matches, so a VALID pattern yields
# status 1 here; only status >= 2 (grep's own diagnostic) is the malformed signal.
pat_status=0
grep -Eq "$patterns" </dev/null 2>/dev/null || pat_status=$?
if [[ "$pat_status" -ge 2 ]]; then
  echo 'leak-scan: invalid pattern set (check tools/leak-scan.local) — refusing to pass' >&2
  exit 2
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

if [[ "${1:-}" == "--history" ]]; then
  # Local opt-in: scan the CONTENT of commits through the same engine as --stdin, so
  # a leak that lived only in an intermediate commit — gone from the current tree and
  # net-zero across a range diff — is still provable. Default is ALL history; an
  # optional range (or any git-log revision args) narrows it: `--history main..HEAD`.
  # The DEFAULT no-arg tracked-tree scan below is unchanged and stays green.
  # --diff-merges=first-parent so a leak introduced only by an evil merge (plain
  # `git log -p` shows no diff for merge commits) is still scanned.
  shift
  status=0
  git log -p --diff-merges=first-parent "$@" | "$here/$(basename "$0")" --stdin || status=$?
  exit "$status"
fi

cd "$here/.."

# A grep/file/xargs diagnostic during a scan must fail the gate CLOSED. Both scans
# below end in `|| true` (needed because xargs/grep return nonzero on the normal
# no-match path), which also swallows a real error (bad ERE, unreadable file) that
# writes to stderr and leaves stdout empty — that would read as "clean". Capture
# each scan's stderr and treat any diagnostic as a hard error.
scan_err="$(mktemp)"
trap 'rm -f "$scan_err"' EXIT

# A tracked file deleted in the worktree but not staged is named by `git ls-files`
# and missing from disk, so both scans below emit "No such file or directory" and
# the gate refuses — correctly, but with a message that blames the scanner for what
# is a plain `rm`. Keep the refusal: skipping tracked paths on the quiet is the
# wrong direction for a publication gate, and a dangling symlink produces the same
# diagnostic while being a genuine finding. Name the fix instead, which is otherwise
# undiscoverable from the message.
refuse() {
  echo "leak-scan: $1 emitted diagnostics — refusing to pass:" >&2
  cat "$scan_err" >&2
  if grep -q 'No such file or directory' "$scan_err"; then
    echo 'leak-scan: hint — if a tracked file was deleted but not staged, `git rm` it (or restore it) and re-run' >&2
  fi
  exit 2
}

# Defence-in-depth: flag binary-looking tracked files early with a clear message.
# file(1) classifies from the head of a file, so this guard alone is bypassable by
# bytes placed past its window — the pattern scan below therefore uses grep -a
# (every byte treated as text) and does not depend on this check for coverage.
# Key on ENCODING, not mime family — file(1) types scripts as
# application/javascript etc.; what matters is that the bytes are greppable text.
# Empty files report encoding "binary" but are trivially safe.
# One reviewed binary asset is exempt: site/og.png (the share card — tool-generated
# from public copy + the public palette, never hand-edited).
# `-L` because the per-crate LICENSE files are symlinks to the root one: without
# it file(1) types the LINK as binary and the guard flags a text file it can read
# perfectly well. Following also keeps this agreeing with the pattern scan below,
# which reads through symlinks whether this one does or not — and a dangling link
# makes file(1) write to stderr, which fails the gate closed.
binaries=$( { git ls-files -z ':!site/og.png' | xargs -0 -r file -L --mime-encoding \
  | grep -Ev ': *(us-ascii|utf-8|ascii)$' \
  | while IFS=: read -r f _; do [[ -s "$f" ]] && echo "$f: non-text encoding"; done; } 2>"$scan_err" || true)
if [[ -s "$scan_err" ]]; then
  refuse 'encoding scan'
fi
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
# The tree scan runs in two tiers. Base public patterns scan every tracked file.
# Estate overlay patterns additionally exempt docs/derivation/reviews/ — the
# clean-room review records are the one sanctioned place that names quarantined
# subjects (each records its grep-for-the-subject verdict), so an overlay tripwire
# on a subject's name would flag the very line proving no leak exists. Base
# patterns (credentials, home paths, key blocks) still cover those records in
# full, and --stdin/--history keep the combined set: history is judged as content,
# not by where a line would land in today's tree.
# Branch on output, not exit status: xargs exits 123 if any batch matches, and a real
# match in one batch plus a clean batch must still fail the gate loudly.
# grep -a, never -I: -I's binary sniff runs over the whole stream, so a NUL past
# file(1)'s window would silently skip the file. Noisy matches from binary bytes
# are the correct failure direction for a publication gate.
# run_scan reports through the global scan_out, never command substitution — a
# $(...) subshell would reduce refuse()'s exit 2 to a swallowed substitution
# status and fail the gate OPEN on a scanner diagnostic.
scan_out=''
run_scan() {
  local label="$1" pats="$2"
  shift 2
  scan_out=$( { git ls-files -z ':!tools/leak-scan.sh' "$@" | xargs -0 -r grep -Ean "$pats"; } 2>"$scan_err" || true)
  if [[ -s "$scan_err" ]]; then
    refuse "$label"
  fi
}
run_scan 'content scan' "$base_patterns"
matches="$scan_out"
if [[ -n "$overlay_patterns" ]]; then
  run_scan 'overlay content scan' "$overlay_patterns" ':!docs/derivation/reviews'
  if [[ -n "$scan_out" ]]; then
    matches="${matches:+$matches$'\n'}$scan_out"
  fi
fi
if [[ -n "$matches" ]]; then
  echo "$matches"
  echo 'leak-scan: private identifiers found' >&2
  exit 1
fi
echo 'leak-scan: clean'
