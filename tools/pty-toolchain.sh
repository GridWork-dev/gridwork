#!/usr/bin/env bash
# Materialize what gwk-pty needs to build: the pinned ghostty source tree and
# its Zig package graph, both under the repo root and both gitignored.
#
# CI calls this and so do you. One implementation, so the offline build that
# passes locally is the same one CI runs — a second copy in the workflow file
# would drift the first time one side gained a flag.
#
# WHY BOTH. libghostty-vt-sys reads GHOSTTY_SOURCE_DIR to skip cloning ghostty,
# and GHOSTTY_ZIG_SYSTEM_DIR to pass `zig build --system`, which turns off Zig's
# own package fetching. Setting only the first still leaves Zig resolving the
# package graph over the network — so "no build-time network" needs both, and
# that is not what the phase's own ruling originally said.
#
# Usage:
#   ./tools/pty-toolchain.sh          # materialize, then print the env to export
#   eval "$(./tools/pty-toolchain.sh --env)"   # just the exports, no work
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
cd "$here/.."

pins="crates/gwk-pty/pins.env"
[[ -f "$pins" ]] || {
  echo "pty-toolchain: $pins is missing — refusing to guess a revision" >&2
  exit 2
}
# shellcheck disable=SC1090
source <(grep -E '^[A-Z_]+=' "$pins")

: "${ZIG_VERSION:?pins.env defines no ZIG_VERSION}"
: "${GHOSTTY_COMMIT:?pins.env defines no GHOSTTY_COMMIT}"
: "${LIBGHOSTTY_VT_VERSION:?pins.env defines no LIBGHOSTTY_VT_VERSION}"

# The third pin is the one cargo owns, so unlike the other two it cannot be READ
# from pins.env — Cargo.toml has to carry the literal. That makes it the single
# number with two homes, which is the shape that drifts: any dependency bump edits
# Cargo.toml and leaves pins.env stating a version nothing checks. Until this
# assertion existed, LIBGHOSTTY_VT_VERSION was a comment wearing a pin's name.
#
# The pattern also holds the pin's SHAPE. A caret range does not match it, so
# relaxing `=0.2.0` to `^0.2.0` fails here rather than quietly letting a patch
# release move the C ABI under a green lockfile.
declared="$(sed -nE 's/^libghostty-vt = "=([0-9][0-9A-Za-z.+-]*)"$/\1/p' Cargo.toml)"
[[ -n "$declared" ]] || {
  echo 'pty-toolchain: Cargo.toml carries no exact libghostty-vt pin (expected: libghostty-vt = "=<version>")' >&2
  exit 1
}
[[ "$declared" == "$LIBGHOSTTY_VT_VERSION" ]] || {
  echo "pty-toolchain: Cargo.toml pins libghostty-vt $declared, pins.env says $LIBGHOSTTY_VT_VERSION" >&2
  exit 1
}

# GHOSTTY_COMMIT IS NOT OURS TO CHOOSE. It is the one pin here that decides an ABI
# rather than a version string, and the check above does not protect it at all.
#
# The chain: we exact-pin the safe wrapper, the wrapper asks for
# `libghostty-vt-sys = "0.2.0"` — a CARET range — and the resolved `-sys` crate is
# what declares which ghostty revision its Rust `extern` declarations were written
# against. So the ABI-critical revision is picked by version resolution, not by us,
# while `pins.env` decides which tree actually gets built. Today they agree by
# coincidence: the wrapper is 0.2.0 and the `-sys` it resolves to is 0.2.1.
#
# Let those diverge and the failure is the silent kind. `GHOSTTY_SOURCE_DIR` points
# `zig build` at OUR tree, so the bindings compile — against a ghostty the
# declarations were not written for. Nothing errors; the struct layouts are just
# wrong. That is exactly what the header of this file warns about, and until now
# nothing enforced it.
#
# Needs the registry populated, so `cargo fetch` has to have run. Refusing is the
# right answer if it has not: materializing a tree we cannot prove is the right one
# is the failure this check exists to prevent.
sys_manifests="$(cargo metadata --format-version 1 --locked --offline 2> /dev/null \
  | jq -r '.packages[] | select(.name == "libghostty-vt-sys") | .manifest_path')" || true
if [[ -z "$sys_manifests" ]]; then
  echo "pty-toolchain: cannot read libghostty-vt-sys from the dependency graph." >&2
  echo "  Run \`cargo fetch --locked\` first. Refusing to materialize a ghostty tree" >&2
  echo "  whose revision has not been checked against the bindings that use it." >&2
  exit 2
fi
if [[ "$(wc -l <<< "$sys_manifests")" -ne 1 ]]; then
  echo "pty-toolchain: the graph resolves more than one libghostty-vt-sys:" >&2
  sed 's/^/  /' <<< "$sys_manifests" >&2
  echo "  One of them decides the ABI and this check cannot tell which." >&2
  exit 1
fi
sys_commit="$(sed -nE 's/^const GHOSTTY_COMMIT: &str = "([0-9a-f]{40})";$/\1/p' \
  "$(dirname "$sys_manifests")/build.rs")"
[[ -n "$sys_commit" ]] || {
  echo "pty-toolchain: could not read GHOSTTY_COMMIT out of $(dirname "$sys_manifests")/build.rs" >&2
  echo "  The crate changed how it names its pinned revision; this check needs updating," >&2
  echo "  and until it is, nothing is verifying the ghostty tree against the bindings." >&2
  exit 1
}
[[ "$sys_commit" == "$GHOSTTY_COMMIT" ]] || {
  echo "pty-toolchain: pins.env builds ghostty $GHOSTTY_COMMIT," >&2
  echo "  but libghostty-vt-sys generated its bindings against $sys_commit." >&2
  echo "  Set GHOSTTY_COMMIT to the latter, or pin a -sys that wants the former." >&2
  exit 1
}

root="$PWD"
src="$root/.ghostty"
pkgs="$root/.zig-packages"

emit_env() {
  echo "export GHOSTTY_SOURCE_DIR=$src"
  echo "export GHOSTTY_ZIG_SYSTEM_DIR=$pkgs/p"
}

if [[ "${1:-}" == "--env" ]]; then
  emit_env
  exit 0
fi

if ! command -v zig > /dev/null; then
  echo "pty-toolchain: zig $ZIG_VERSION is not on PATH." >&2
  echo "  Install it from https://ziglang.org/download/$ZIG_VERSION/ — distro" >&2
  echo "  packages are already past this version and will not work." >&2
  exit 2
fi

# Ghostty pins a MINIMUM Zig version, so a newer Zig is not automatically fine:
# 0.15 → 0.16 was a breaking language change. Warn rather than block, because a
# contributor pinning a patch release deliberately is not an error.
have="$(zig version)"
if [[ "$have" != "$ZIG_VERSION" ]]; then
  echo "pty-toolchain: zig $have on PATH, pins.env says $ZIG_VERSION — continuing, but this is the first thing to suspect if the build fails" >&2
fi

if [[ -f "$src/.pinned-at" ]] && [[ "$(cat "$src/.pinned-at")" == "$GHOSTTY_COMMIT" ]]; then
  echo "pty-toolchain: ghostty already at $GHOSTTY_COMMIT"
else
  rm -rf "$src"
  git init -q "$src"
  git -C "$src" remote add origin https://github.com/ghostty-org/ghostty.git
  git -C "$src" fetch -q --depth 1 origin "$GHOSTTY_COMMIT"
  git -C "$src" checkout -q FETCH_HEAD
  # Assert the revision. A fetch that quietly landed somewhere else yields
  # bindings that no longer match the C ABI they were generated against, and
  # nothing downstream notices until it segfaults.
  got="$(git -C "$src" rev-parse HEAD)"
  [[ "$got" == "$GHOSTTY_COMMIT" ]] || {
    echo "pty-toolchain: ghostty is at $got, expected $GHOSTTY_COMMIT" >&2
    exit 1
  }
  echo "$GHOSTTY_COMMIT" > "$src/.pinned-at"
fi

# `--fetch=all`, not `--fetch`. Plain `--fetch` resolves only non-lazy
# dependencies, while `zig build --system` resolves the whole graph eagerly — so
# the lazy ones come up missing at exactly the point they can no longer be
# fetched. That failure reads as "lazy dependency package not found" and says
# nothing about which flag was wrong.
( cd "$src" && zig build --fetch=all --global-cache-dir "$pkgs" )

echo "pty-toolchain: ready. Export these, or use \`eval \"\$($0 --env)\"\`:"
emit_env
