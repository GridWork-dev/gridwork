#!/usr/bin/env bash
# Clean-room gate: a change touching an engine-adjacent path must carry a
# second-reader record bound to the exact content that was reviewed, and every
# gated source file in it must say what it was derived from.
#
# Gated paths are the prefixes in .github/cleanroom-paths.txt (CLEANROOM.md rule
# 2). The record is docs/derivation/reviews/<subject>.md.
#
# WHY A CONTENT DIGEST AND NOT THE HEAD SHA. A record naming the PR head cannot
# exist: committing the record changes the head. The subject digest is computed
# from GATED files plus the registry rows their markers cite, so adding the
# record — which is neither — does not move it, while a later edit to a gated
# file, or to a row a past review vouched for, does. That is what stops a review
# from being recycled across a rewrite of the thing it reviewed — including the
# registry half of a citation, which is half of what the reviewer checked.
#
# WHAT THIS PROVES. That a review ran against exactly this content, that its
# record was written at commit time rather than reconstructed later, and that
# every gated source file names a source the record also names. It does NOT prove
# the reviewer was independent; on a single-maintainer repo no status check can,
# and CLEANROOM.md rule 4 says so in the same words. Nor does it prove a marker is
# TRUE — a marker can cite a note the code did not actually come from, and only a
# reader catches that. It proves the claim was made and cross-referenced, which is
# what makes a false one attributable. A gate claiming more would be false, and a
# false clean-room claim is worse than an honest narrow one.
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
  --markers) ;;
  *)
    echo "usage: cleanroom-gate.sh [--subject | --markers] < changed-paths" >&2
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

# Which gated files owe a `Derivation:` marker. Restricted to code, because a
# marker demanded of a Cargo.toml or a LICENSE buys nothing and teaches people to
# write filler markers to get past the gate — and a filler marker is worse than no
# marker, since it looks like provenance.
#
# vendor/ and fixtures/ are excluded on the same reasoning inverted: verbatim
# upstream source and captured third-party test data are not clean-room-derived
# work, they are dependencies. Demanding we annotate someone else's file with what
# we took from it is a category error.
#
# ponytail: extension allowlist, not a language detector. Add extensions when a
# gated crate starts carrying them.
owes_marker() {
  local f="$1"
  [[ -f "$f" ]] || return 1 # deleted by this change; nothing to annotate
  [[ "$f" == */vendor/* || "$f" == */fixtures/* ]] && return 1
  case "$f" in
    *.rs | *.zig | *.c | *.h | *.cpp | *.hpp) return 0 ;;
    *) return 1 ;;
  esac
}

# The two registries a citation may resolve in (CLEANROOM.md rule 3). The order
# is fixed because row text feeds the subject digest below.
registries=(docs/derivation/CAPTURES.md docs/derivation/SPECS.md)

# The table-cell match for an ID, dots escaped so `CAP.001` cannot match a
# `CAP-001` row. Cell match, so a passing mention in prose does not register an
# ID. Backticks optional — a markdown table reads better with them and the
# registries should not have to choose between legible and matchable.
cell_regex() {
  printf '^\\|[[:space:]]*`?%s`?[[:space:]]*\\|' "${1//./\\.}"
}

# Does this citation resolve? Rule 3 admits exactly two kinds of permitted source
# — a public specification or a registered observation — so a citation resolves
# iff it is an ID in one of the two registries.
#
# NOT a path. Rule 4's `references` check bans citing a capture by path outright,
# so a marker naming a file would be a violation dressed as provenance. That is
# also why this does not cross-reference the review record: the record is bound to
# the content by digest already, and the content includes these markers, so
# demanding the record restate them proves nothing the digest has not.
resolves() {
  local id="$1" reg
  # An ID is alphanumerics, dashes, dots and underscores. Without this, the
  # em-dash placeholder in an empty registry table is itself a matchable ID.
  [[ "$id" =~ ^[A-Za-z0-9._-]+$ ]] || return 1
  for reg in "${registries[@]}"; do
    [[ -f "$reg" ]] || continue
    if grep -qE "$(cell_regex "$id")" "$reg"; then
      return 0
    fi
  done
  return 1
}

# `<comment> Derivation: <ID>[ §section] — <what was taken>` on stdin → the IDs.
# One sed shared by the marker check and the subject computation, so the two can
# never disagree about what counts as a marker. The description is required
# (that trailing `[^[:space:]]`), so a bare ID is not a marker. The separator
# before it is deliberately unconstrained: pinning an em-dash would make the
# gate's verdict depend on the CI runner's locale.
cited_ids() {
  sed -nE 's@^[[:space:]]*(//|#)[[:space:]]*Derivation:[[:space:]]+([^[:space:]]+)[[:space:]]+[^[:space:]].*$@\2@p'
}

# CLEANROOM.md rule 3, made mechanical. Derived code names its permitted source in
# the line above it. Until now rule 3 was enforced only by a reviewer ticking a
# box — which is the check that cannot fail, one layer up.
check_markers() {
  local f cited id bad=0

  for f in "$@"; do
    owes_marker "$f" || continue

    mapfile -t cited < <(cited_ids < "$f")

    if [[ ${#cited[@]} -eq 0 ]]; then
      echo "cleanroom-gate: $f is under the clean-room gate and carries no 'Derivation:' marker" >&2
      bad=1
      continue
    fi

    local unresolved=0
    for id in "${cited[@]}"; do
      if ! resolves "$id"; then
        echo "cleanroom-gate: $f cites '$id', which is in neither CAPTURES.md nor SPECS.md" >&2
        unresolved=1
      fi
    done
    if [[ $unresolved -ne 0 ]]; then
      bad=1
      continue
    fi

    echo "cleanroom-gate: $f cites ${cited[*]}"
  done

  if [[ $bad -ne 0 ]]; then
    cat >&2 <<'EOF'

CLEANROOM.md rule 3: every non-obvious terminal behavior names the permitted
source it came from, immediately above the derived construct:

  // Derivation: ECMA-48 §8.3.20 — CUF advances the active position by Pn, default 1
  // Derivation: CAP-001 — the wrap-at-column-N behavior this observes

The citation is an ID registered in docs/derivation/SPECS.md (public
specifications) or docs/derivation/CAPTURES.md (observed captures). Never a file
path — rule 4's `references` check bans citing a capture by path.
EOF
    return 1
  fi
}

# Every capture this repository publishes is registered under its own digest.
#
# CAPTURES.md names no paths, so this cannot map a row to a file. It asserts the
# weaker and sufficient thing: each committed capture's sha256 appears as some
# row. Edit the bytes and the digest stops appearing, and that is the entire
# reason a self-made capture is allowed to have its location named at all — the
# registry's objection to paths is that a claim resting on one can come to point
# at different bytes without saying so, and this is what makes it say so.
#
# Deliberately NOT driven by the changed-path list. A capture lives under docs/,
# which is not a gated prefix, so a change touching only a capture file reaches
# the early exit below and is never looked at. That is precisely the edit worth
# catching, so this runs on every invocation of the real gate.
#
# ponytail: everything under the directory is treated as a capture. If it ever
# needs a README, give this an extension allowlist rather than a skip-list of
# names.
check_captures() {
  local dir="docs/derivation/captures" reg="docs/derivation/CAPTURES.md"
  local f sum bad=0
  [[ -d "$dir" ]] || return 0
  # `find`, not `"$dir"/*`. The glob skips dotfiles under bash's default settings
  # and does not descend, so a capture named `.x`, or one in a subdirectory, was
  # invisible to a check whose entire claim is that nothing in here goes
  # unregistered. The round-nine reader found it by dropping a dotfile in and
  # watching the gate pass — the same defect this function was added to fix, one
  # layer narrower, which is the argument for asking the filesystem what is there
  # rather than describing it in a pattern.
  #
  # `! -type d` rather than `-type f`, so a symlink is followed and hashed rather
  # than silently skipped; `-print0` so a newline in a filename cannot end the
  # loop early. A dangling symlink has no bytes to smuggle and falls out at the
  # `-f` test below.
  while IFS= read -r -d '' f; do
    [[ -f "$f" ]] || continue
    sum="$(sha256sum "$f" | cut -d' ' -f1)"
    # Anchored to the hash column, the way resolves() anchors to the ID column
    # and for the same reason: a mention is not a registration. The lookup was
    # once `^\|.*${sum}.*\|` — the digest appearing anywhere in a table row —
    # and the round-ten reader registered an unregistered capture against it
    # twice: the real digest quoted in a row's description while the hash cell
    # held a placeholder, and a hash cell padded around the real digest. The
    # row's hash field has to BE the hash, not contain it.
    if ! grep -qE "^\|[^|]*\|[[:space:]]*\`?${sum}\`?[[:space:]]*\|" "$reg"; then
      echo "cleanroom-gate: $f is not registered in $reg under its own sha256" >&2
      echo "cleanroom-gate:   the file hashes to $sum, which no row's hash cell carries" >&2
      bad=1
    fi
  done < <(find "$dir" ! -type d -print0)
  if [[ $bad -ne 0 ]]; then
    cat >&2 <<'EOF'

A capture is cited by digest, never by path, so its bytes and its row have to
stay the same thing. Either the file was edited after it was registered — in
which case it is no longer the observation anything cites, and re-registering it
means a new row, not an edited hash — or it was added without a row.
EOF
    return 1
  fi
}

if [[ "$mode" == "check" ]]; then
  check_captures
fi

input_paths=()
while IFS= read -r f; do
  [[ -n "$f" ]] || continue
  input_paths+=("$f")
done

touched=()
for f in "${input_paths[@]}"; do
  for p in "${prefixes[@]}"; do
    if [[ "$f" == "$p"* ]]; then
      touched+=("$f")
      break
    fi
  done
done

# A registry edit is an edit to the reviewed surface. The subject below folds in
# the rows a change's markers cite, but a change touching ONLY a registry would
# reach the empty-touched exit and never be asked for anything — which is
# exactly how a row a past review vouched for would be rewritten in silence
# (the round-ten record demonstrated the shape). So when the input names a
# registry, diff that registry's ROW LINES against CLEANROOM_BASE and treat
# every gated file at HEAD citing a changed row's ID as touched. A row nothing
# cites expands nothing: pre-seeding stays as cheap as the registries promise.
touched_registries=()
for f in "${input_paths[@]}"; do
  for reg in "${registries[@]}"; do
    if [[ "$f" == "$reg" ]]; then
      touched_registries+=("$reg")
      break
    fi
  done
done

if [[ ${#touched_registries[@]} -gt 0 ]]; then
  if [[ -z "${CLEANROOM_BASE:-}" ]]; then
    cat >&2 <<'EOF'
cleanroom-gate: the change touches a citation registry and there is no
CLEANROOM_BASE to diff its rows against — refusing to pass. CI exports the
merge base; locally:

  git diff --name-only main...HEAD | CLEANROOM_BASE="$(git merge-base main HEAD)" ./tools/cleanroom-gate.sh

A registry edit judged without a base would be waved through blind, and a row
a past review vouched for is exactly the thing that must not move in silence.
EOF
    exit 2
  fi
  if ! git rev-parse --verify --quiet "${CLEANROOM_BASE}^{commit}" > /dev/null; then
    echo "cleanroom-gate: CLEANROOM_BASE '$CLEANROOM_BASE' is not a commit — refusing to pass" >&2
    exit 2
  fi
  for reg in "${touched_registries[@]}"; do
    while IFS= read -r id; do
      [[ -n "$id" ]] || continue
      while IFS= read -r hit; do
        f="${hit#HEAD:}"
        for p in "${prefixes[@]}"; do
          if [[ "$f" == "$p"* ]]; then
            echo "cleanroom-gate: $reg row '$id' changed against $CLEANROOM_BASE — $f cites it, treating it as touched" >&2
            touched+=("$f")
            break
          fi
        done
      done < <(git grep -lE "Derivation:[[:space:]]+${id//./\\.}([[:space:]]|\$)" HEAD -- 2>/dev/null || true)
    done < <(
      # Row lines present on exactly one side of the base..HEAD pair are the
      # changed set — an edited row shows up once per side, its ID once after
      # the sort. A registry absent at the base is all-new rows, which is the
      # pre-seed case and expands nothing unless something already cites them.
      LC_ALL=C comm -3 \
        <(git show "$CLEANROOM_BASE:$reg" 2>/dev/null | grep -E '^\|' | LC_ALL=C sort -u || true) \
        <(git show "HEAD:$reg" 2>/dev/null | grep -E '^\|' | LC_ALL=C sort -u || true) |
        sed -e 's/^\t//' |
        sed -nE 's@^\|[[:space:]]*`?([A-Za-z0-9._-]+)`?[[:space:]]*\|.*@\1@p' |
        LC_ALL=C sort -u
    )
  done
  if [[ ${#touched[@]} -gt 0 ]]; then
    mapfile -t touched < <(printf '%s\n' "${touched[@]}" | LC_ALL=C sort -u)
  fi
fi

if [[ ${#touched[@]} -eq 0 ]]; then
  # Both non-default modes are fed a deliberate input by their caller. An empty
  # gated set means that input missed, so the run proved nothing — say so rather
  # than reporting the clean-diff success, which is a different fact.
  if [[ "$mode" == "--subject" || "$mode" == "--markers" ]]; then
    echo "cleanroom-gate: no engine-adjacent paths in the input" >&2
    exit 1
  fi
  echo "cleanroom-gate: clean — no engine-adjacent paths touched"
  exit 0
fi

if [[ "$mode" == "--markers" ]]; then
  check_markers "${touched[@]}"
  exit $?
fi

# The subject: every touched gated path paired with its blob hash at HEAD, sorted
# so input order cannot change the digest, followed by the registry half of every
# citation those files make — the row text, at HEAD, for each ID their markers
# cite. A review checks that a marker and its row AGREE; binding only the file
# half would leave the row free to move under a record that vouched for the pair.
# A path that no longer exists at HEAD was deleted by this change and still needs
# review, so it contributes a marker rather than being dropped; an ID whose row
# has gone missing contributes one the same way.
#
# LC_ALL=C IS LOAD-BEARING. `sort` is locale-collated, and the digest is
# order-dependent, so without a fixed collation the same content hashes to two
# different subjects on two different machines. That is not theoretical: a
# developer box under en_US.UTF-8 ordered `pins.env` before `README.md` (case
# folded, p < r) while the CI runner under C ordered `README.md` first
# (`R` 0x52 < `p` 0x70), and the record written from one was rejected by the
# other. A gate whose verdict depends on $LANG is not a gate.
subject="$(
  {
    for f in $(printf '%s\n' "${touched[@]}" | LC_ALL=C sort -u); do
      if obj="$(git rev-parse --verify --quiet "HEAD:$f")"; then
        printf '%s %s\n' "$obj" "$f"
      else
        printf 'deleted %s\n' "$f"
      fi
    done
    for id in $(
      for f in $(printf '%s\n' "${touched[@]}" | LC_ALL=C sort -u); do
        git show "HEAD:$f" 2>/dev/null || true
      done | cited_ids | LC_ALL=C sort -u
    ); do
      rows="$(
        for reg in "${registries[@]}"; do
          git show "HEAD:$reg" 2>/dev/null || true
        done | grep -E "$(cell_regex "$id")" || true
      )"
      if [[ -n "$rows" ]]; then
        sed 's/^/row /' <<< "$rows"
      else
        printf 'unresolved-row %s\n' "$id"
      fi
    done
  } | sha256sum | cut -d' ' -f1
)"

if [[ "$mode" == "--subject" ]]; then
  echo "$subject"
  exit 0
fi

record="docs/derivation/reviews/$subject.md"

echo "cleanroom-gate: engine-adjacent paths touched:"
# Same collation as the digest, so what a reader sees listed is the order that
# was actually hashed.
printf '  %s\n' $(printf '%s\n' "${touched[@]}" | LC_ALL=C sort -u)
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

# CLEANROOM.md rule 4's four checks, each ticked. Keyed on the leading token so
# the prose can be edited in the template without silently disarming the gate.
for check in citations framing references leak; do
  if ! grep -qE "^- \[x\] $check\b" "$record"; then
    echo "cleanroom-gate: $record does not record a pass on the '$check' check" >&2
    exit 1
  fi
done

echo "cleanroom-gate: second-reader record present at $record"

check_markers "${touched[@]}"
