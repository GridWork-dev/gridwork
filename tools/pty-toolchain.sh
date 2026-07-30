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
