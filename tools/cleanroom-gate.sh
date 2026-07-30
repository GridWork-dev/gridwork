#!/usr/bin/env bash
# Clean-room gate: a change touching an engine-adjacent path must carry a
# second-reader record bound to the exact content that was reviewed.
#
# Gated paths are the prefixes in .github/cleanroom-paths.txt (CLEANROOM.md rule
# 2). The record is docs/derivation/reviews/<subject>.md.
#
# WHY A CONTENT DIGEST AND NOT THE HEAD SHA. A record naming the PR head cannot
# exist: committing the record changes the head. The subject digest is computed
# only from GATED files, so adding the record — which is not itself gated — does
# not move it, while any later edit to a gated file does. That is what stops a
# review from being recycled across a rewrite of the thing it reviewed.
#
# WHAT THIS PROVES. That a review ran against exactly this content and its record
# was written at commit time rather than reconstructed later. It does NOT prove
# the reviewer was independent; on a single-maintainer repo no status check can,
# and CLEANROOM.md rule 4 says so in the same words. A gate claiming otherwise
# would be false, and a false clean-room claim is worse than an honest narrow one.
#
# Reads the changed-path list on stdin, one path per line, so CI can feed it a
# real diff and a seeded violation through the same code path — the house rule
# from tools/leak-scan.sh, for the same reason: a gate that cannot fail is a
# defect.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
cd "$here/.."

mode="${1:-check}"
case "$mode" in
  check | --subject) ;;
  *)
    echo "usage: cleanroom-gate.sh [--subject] < changed-paths" >&2
    exit 2
    ;;
esac

paths_file=".github/cleanroom-paths.txt"
if [[ ! -f "$paths_file" ]]; then
  echo "cleanroom-gate: $paths_file is missing — refusing to pass" >&2
  exit 2
fi

# Prefix match, per the file's own header. A prefix set that reads empty would
# make every change look unGATED, so treat it as a broken gate rather than a
# clean one.
prefixes=()
while IFS= read -r line; do
  prefixes+=("$line")
done < <(grep -Ev '^[[:space:]]*(#|$)' "$paths_file" || true)
if [[ ${#prefixes[@]} -eq 0 ]]; then
  echo "cleanroom-gate: $paths_file lists no prefixes — refusing to pass" >&2
  exit 2
fi

touched=()
while IFS= read -r f; do
  [[ -n "$f" ]] || continue
  for p in "${prefixes[@]}"; do
    if [[ "$f" == "$p"* ]]; then
      touched+=("$f")
      break
    fi
  done
done

if [[ ${#touched[@]} -eq 0 ]]; then
  [[ "$mode" == "--subject" ]] && {
    echo "cleanroom-gate: no engine-adjacent paths in the input" >&2
    exit 1
  }
  echo "cleanroom-gate: clean — no engine-adjacent paths touched"
  exit 0
fi

# The subject: every touched gated path paired with its blob hash at HEAD, sorted
# so input order cannot change the digest. A path that no longer exists at HEAD
# was deleted by this change and still needs review, so it contributes a marker
# rather than being dropped.
subject="$(
  for f in $(printf '%s\n' "${touched[@]}" | sort -u); do
    if obj="$(git rev-parse --verify --quiet "HEAD:$f")"; then
      printf '%s %s\n' "$obj" "$f"
    else
      printf 'deleted %s\n' "$f"
    fi
  done | sha256sum | cut -d' ' -f1
)"

if [[ "$mode" == "--subject" ]]; then
  echo "$subject"
  exit 0
fi

record="docs/derivation/reviews/$subject.md"

echo "cleanroom-gate: engine-adjacent paths touched:"
printf '  %s\n' $(printf '%s\n' "${touched[@]}" | sort -u)
echo "cleanroom-gate: subject $subject"

if [[ ! -f "$record" ]]; then
  cat >&2 <<EOF
cleanroom-gate: no second-reader record at $record

This change touches paths under the clean-room gate, so CLEANROOM.md rule 4
applies: a fresh-context review of this exact content, recorded before it reaches
main. Write the record and commit it in this branch:

  cp docs/derivation/reviews/TEMPLATE.md $record

then fill it in. docs/derivation/reviews/README.md has the procedure.
EOF
  exit 1
fi

# The record's own shape. Without this the gate would accept an empty file, which
# is the green-checkbox failure the seeded violation exists to disprove.
if ! grep -qx "subject: $subject" "$record"; then
  echo "cleanroom-gate: $record does not declare 'subject: $subject'" >&2
  exit 1
fi
if ! grep -qE '^reviewer: .*[^[:space:]]' "$record"; then
  echo "cleanroom-gate: $record has no 'reviewer:' line naming the reviewing lane" >&2
  exit 1
fi

# CLEANROOM.md rule 4's three checks, each ticked. Keyed on the leading token so
# the prose can be edited in the template without silently disarming the gate.
for check in citations framing leak; do
  if ! grep -qE "^- \[x\] $check\b" "$record"; then
    echo "cleanroom-gate: $record does not record a pass on the '$check' check" >&2
    exit 1
  fi
done

echo "cleanroom-gate: second-reader record present at $record"
