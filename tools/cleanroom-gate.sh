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
#
# The comment forms are `//`, `//!`, `///` and `#`. The doc-comment forms are
# here because a marker in a doc comment is still a marker, and every consumer
# of this function — the never-both check included — can only see what it sees:
# with `//!` unrecognized, a module-doc citation beside a `// Derivation: none`
# read as a bare declaration and passed, which is exactly the
# reviewer-stops-at-the-none failure the never-both check exists to prevent.
# `///` had the same hole one form over — an outer doc comment documents the
# very item a derived construct usually is, so it is where a marker naturally
# lands, and a `Derivation:` line written there was invisible: unmarked standing
# alone, a bare `none` standing beside one. The slash run is unbounded (`////`
# and deeper match too): any such line still reads as a comment to a human, and
# a form the extractor cannot see is a line the never-both check cannot judge —
# the safe direction is to see more, never less. No `/*`-style form: the
# extractor has never recognized one, and every check aligns with this one
# pattern rather than growing its own.
#
# The description must contain at least one ALPHANUMERIC: a reason spelled
# entirely in punctuation (`Derivation: none — —`) satisfied the
# something-follows check while saying nothing, which made the mandatory
# reason optional in practice.
cited_ids() {
  sed -nE 's@^[[:space:]]*(//[/!]*|#)[[:space:]]*Derivation:[[:space:]]+([^[:space:]]+)[[:space:]]+[^[:alnum:]]*[[:alnum:]].*$@\2@p'
}

# The one ID that is a DECLARATION rather than a citation: `Derivation: none — <why>`
# says this file derives nothing, so there is no permitted source to name.
#
# It exists because owes_marker() keys on file extension, not content: a gated
# crate's skeleton — doc comments, no process spawned, no byte parsed, no session
# supervised — owed a marker it had nothing truthful to fill in, and the only ways
# out were a false citation or leaving the crate ungated. Both are worse than
# saying so. This is the same bargain the rest of rule 3 already makes: the gate
# proves a claim was MADE and is attributable; rule 4's reader is what makes it
# true. A `none` is exactly as reviewable as a wrong section number, and more
# visible than either.
#
# The reason is mandatory, and free: cited_ids' trailing `[^[:space:]]` means a
# bare `Derivation: none` is not a marker at all, so the file reads as unmarked
# and the gate rejects it. An unexplained `none` is the bypass this form would
# otherwise be.
NONE_ID='none'

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

    # Split the declaration off from real citations before anything resolves it.
    local declares_none=0 real=()
    for id in "${cited[@]}"; do
      if [[ "$id" == "$NONE_ID" ]]; then
        declares_none=1
      else
        real+=("$id")
      fi
    done

    # The two cannot both be true of one file. A `none` that sits beside a real
    # citation is not a declaration, it is a contradiction — and the direction it
    # fails matters: a reviewer skimming for markers sees the `none`, believes the
    # file derives nothing, and never checks the citation it is standing next to.
    if [[ $declares_none -eq 1 && ${#real[@]} -gt 0 ]]; then
      echo "cleanroom-gate: $f declares 'Derivation: none' while also citing ${real[*]} — a file derives nothing or it names its sources, never both" >&2
      bad=1
      continue
    fi

    if [[ $declares_none -eq 1 ]]; then
      echo "cleanroom-gate: $f declares no derivation"
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

If the file genuinely derives NOTHING — a skeleton, a re-export, a config module
that spawns no process, parses no byte, and supervises no session — say so
instead of inventing a citation:

  // Derivation: none — skeleton only: no process spawned, no byte parsed

The reason is required: a bare `Derivation: none` is not a marker and this gate
will keep rejecting the file. A `none` may not sit beside a real citation — a
file derives nothing or it names its sources, never both. Rule 4's reader checks
the declaration exactly as they check a section number.

A behavior with no citable permitted source is an escalation, not a `none`.
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

not_engine_file=".github/cleanroom-not-engine.txt"

# Every `crates/` directory is CLASSIFIED: it matches a gated prefix, or it
# carries a line in the not-engine allowlist. Never neither.
#
# The hole this closes is not a missing prefix, it is a missing decision. The
# prefix list said what was gated and nothing said what the rest was, so a new
# crate's coverage came down to whether its name happened to start with a listed
# string — `gwk-pty-host` gates, `gwk-host` does not, and nothing anywhere would
# have said so. Requiring a positive classification turns that from an omission
# nobody notices into a line someone has to write and a reader can disagree with.
#
# Deliberately NOT driven by the changed-path list, for the same reason as
# check_captures: the change that adds an unclassified crate is exactly the
# change whose paths reach no gated prefix, so a diff-driven version would be
# blind to the only thing it is for.
#
# Three ways to fail, and the second and third matter as much as the first:
# unclassified (the crate nobody decided about); a listed path that is not a
# directory (a stale line is how a future crate silently inherits a
# classification written for a different one — and how a crate could be
# pre-classified as not-engine before it exists, which is the coin flip again
# with the coin tossed early); and a path both gated and listed (the two files
# disagreeing, which makes the record worth nothing whichever one is right).
check_crate_classification() {
  local dir="crates"
  [[ -d "$dir" ]] || return 0

  if [[ ! -f "$not_engine_file" ]]; then
    echo "cleanroom-gate: $not_engine_file is missing — refusing to pass" >&2
    return 1
  fi

  # `<path> <whitespace> <rationale>`. The rationale is REQUIRED by the pattern
  # — that trailing `[^[:space:]]` — the same way a Derivation: marker requires
  # a description, and for the same reason: a bare path is a classification
  # nobody had to defend, and one nobody had to defend is one nobody thought
  # about. A line without one does not parse, so its crate reads as
  # unclassified and the gate says so by name.
  local listed=()
  mapfile -t listed < <(
    sed -nE 's@^[[:space:]]*([^#[:space:]]+)[[:space:]]+[^[:space:]].*$@\1@p' "$not_engine_file"
  )

  local bad=0 d p l gated found
  while IFS= read -r -d '' d; do
    gated=0
    for p in "${prefixes[@]}"; do
      if [[ "$d" == "$p"* ]]; then
        gated=1
        break
      fi
    done
    found=0
    for l in ${listed[@]+"${listed[@]}"}; do
      if [[ "$l" == "$d" ]]; then
        found=1
        break
      fi
    done

    if [[ $gated -eq 1 && $found -eq 1 ]]; then
      echo "cleanroom-gate: $d is both gated and listed as not-engine — the two files disagree" >&2
      bad=1
    elif [[ $gated -eq 0 && $found -eq 0 ]]; then
      echo "cleanroom-gate: $d matches no gated prefix and no not-engine line — unclassified" >&2
      bad=1
    fi
  done < <(find "$dir" -mindepth 1 -maxdepth 1 -type d -print0 | LC_ALL=C sort -z)

  for l in ${listed[@]+"${listed[@]}"}; do
    if [[ ! -d "$l" ]]; then
      echo "cleanroom-gate: $not_engine_file lists '$l', which is not a directory" >&2
      bad=1
    fi
  done

  if [[ $bad -ne 0 ]]; then
    cat >&2 <<EOF

CLEANROOM.md rule 2: every crate is on one side of the gate or the other, and
which side is a decision that gets written down. Either add a prefix to
.github/cleanroom-paths.txt — at whatever module granularity keeps the tax off
lens code — or add a line to $not_engine_file saying, in one sentence, why this
code neither supervises an engine process or a PTY session nor emits or parses
terminal-protocol bytes.

Leaving it out is not neutral. It is the crate being ungated because nobody
looked, which is indistinguishable from the crate being ungated on purpose.
EOF
    return 1
  fi
}

# A crate that declares a DIRECT `gwk-pty` dependency is gated, by that fact and
# not by its name.
#
# This is the other half of the coin flip. The classification check above makes
# someone decide; this one takes the decision away where the answer is already
# known — a crate linking the PTY engine has the engine's types, its session
# handles and its terminal bytes in scope, and whether the gate reaches it
# cannot be left to whether somebody called it `gwk-pty-host` or `gwk-host`.
# `crates/gwk-adapter-` covers the three that do today, so this adds no rows;
# it makes the fourth one impossible to add quietly.
#
# TRANSITIVE dependencies are deliberately NOT covered. gwk-parity gets the
# engine's types through the adapters and is not gated wholesale for it — the
# category rule is module-granular precisely so a harness's pure half stays
# cheap. Direct is the line because declaring the dependency is the act of
# reaching for the engine, and it is the act a manifest records.
#
# ponytail: greps manifests rather than asking cargo. This job has no Rust
# toolchain — it is a checkout and a bash script — and adding one to read
# `cargo metadata` would cost minutes per run to learn what four lines of TOML
# already say. The four shapes a direct dependency can take are all matched
# below; the discovery is asserted non-empty so a fifth shape cannot make this
# pass by finding nothing.
check_pty_dependents() {
  local bad=0 found=0 manifest crate p gated

  for manifest in crates/*/Cargo.toml xtask/Cargo.toml; do
    [[ -f "$manifest" ]] || continue
    crate="${manifest%/Cargo.toml}"
    # `gwk-pty = ...` / `gwk-pty.workspace = true` / `[dependencies.gwk-pty]` /
    # a rename (`foo = { package = "gwk-pty" }`). The rename matters most: it is
    # the one shape that hides the dependency from a reader skimming for the
    # name, which is exactly the reader this gate stands in for.
    #
    # Every arm accepts an optionally QUOTED key, and the rename arm accepts
    # either quote character, because TOML spells a key three ways — bare, basic
    # ("gwk-pty"), and literal ('gwk-pty') — and cargo accepts all three. An
    # earlier version matched only the bare key and only a double-quoted rename,
    # so `hidden = { package = 'gwk-pty', path = '../gwk-pty' }` linked the PTY
    # engine and passed: no prefix row, no second-reader record, no markers. A
    # check that knows some spellings of a name is a check with an undocumented
    # bypass, and this one was a single keystroke wide.
    grep -qE '^[[:space:]]*"?'"'"'?gwk-pty"?'"'"'?[[:space:]]*[.=]|^\[[^]]*dependencies\."?'"'"'?gwk-pty"?'"'"'?\]|package[[:space:]]*=[[:space:]]*["'"'"']gwk-pty["'"'"']' \
      "$manifest" || continue
    # Its own manifest names it without depending on it.
    [[ "$crate" == "crates/gwk-pty" ]] && continue
    found=$((found + 1))

    gated=0
    for p in "${prefixes[@]}"; do
      if [[ "$crate" == "$p"* ]]; then
        gated=1
        break
      fi
    done
    if [[ $gated -eq 0 ]]; then
      echo "cleanroom-gate: $crate declares a direct gwk-pty dependency and matches no gated prefix" >&2
      bad=1
    fi
  done

  # The positive control, inline. Every seed for this check is a violation, so
  # deleting the grep would leave a check that passes because it finds nothing —
  # the same green-because-it-could-not-look failure the whole job exists for.
  # Three crates declare the dependency today; zero means the detection broke,
  # not that the tree got cleaner.
  if [[ $found -eq 0 ]]; then
    echo "cleanroom-gate: no crate declares a gwk-pty dependency — the detection is broken, not the tree" >&2
    bad=1
  fi

  if [[ $bad -ne 0 ]]; then
    cat >&2 <<'EOF'

A crate that links the PTY engine holds its types, its session handles and its
terminal bytes. Whether the clean-room gate reaches it is not allowed to depend
on the name someone picked: add the crate's prefix to
.github/cleanroom-paths.txt in the same commit that adds the dependency.
EOF
    return 1
  fi
}

if [[ "$mode" == "check" ]]; then
  check_captures
  check_crate_classification
  check_pty_dependents
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

  git diff --no-renames --name-only main...HEAD | CLEANROOM_BASE="$(git merge-base main HEAD)" ./tools/cleanroom-gate.sh

A registry edit judged without a base would be waved through blind, and a row
a past review vouched for is exactly the thing that must not move in silence.
EOF
    exit 2
  fi
  if ! git rev-parse --verify --quiet "${CLEANROOM_BASE}^{commit}" > /dev/null; then
    echo "cleanroom-gate: CLEANROOM_BASE '$CLEANROOM_BASE' is not a commit — refusing to pass" >&2
    exit 2
  fi
  # A floor, not caller-proofing: CI computes its own merge base, so this
  # guards the local invocation. Any resolvable commit used to pass — HEAD
  # itself included, which makes the row diff vacuously empty. The base must
  # be a PROPER ancestor: strictly behind HEAD, on its history.
  if [[ "$(git rev-parse "${CLEANROOM_BASE}^{commit}")" == "$(git rev-parse HEAD)" ]]; then
    echo "cleanroom-gate: CLEANROOM_BASE is HEAD itself — an empty row diff proves nothing; refusing to pass" >&2
    exit 2
  fi
  if ! git merge-base --is-ancestor "$CLEANROOM_BASE" HEAD; then
    echo "cleanroom-gate: CLEANROOM_BASE '$CLEANROOM_BASE' is not an ancestor of HEAD — refusing to pass" >&2
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
      # `none` is a declaration, not a citation: there is no registry row to bind,
      # so it contributes nothing here. Without this it would hash as
      # `unresolved-row none` — stable, but a lie in the one artifact whose whole
      # job is to say exactly what the reviewer read. The declaring file's own blob
      # is already in the digest from the loop above, so changing the stated reason
      # still moves the subject and still re-opens review.
      [[ "$id" == "$NONE_ID" ]] && continue
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
