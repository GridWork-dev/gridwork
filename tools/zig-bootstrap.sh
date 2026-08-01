#!/usr/bin/env bash
# Install the pinned Zig so `gwk-pty` can be built from a clean clone.
#
# WHY THIS EXISTS. Every distro that ships Zig is already past the pinned
# version — ghostty pins 0.15.2 as a MINIMUM and 0.15 to 0.16 was a breaking
# language change, so a system Zig is the wrong Zig by default rather than by
# bad luck. Without this, a contributor's first `cargo test -p gwk-pty` fails
# with a message about a version they then have to go and find by hand.
#
# WHY CI DOES NOT USE IT. `mlugg/setup-zig` verifies its download against
# upstream's minisign signature, which is strictly better than a checksum we
# wrote down. This script is for humans on a laptop, and its checksum lives in
# pins.env next to the version it belongs to.
#
# Usage:
#   ./tools/zig-bootstrap.sh                 # install under ~/.local/zig
#   ZIG_INSTALL_ROOT=/opt ./tools/zig-bootstrap.sh
#   eval "$(./tools/zig-bootstrap.sh --env)" # just print the PATH export
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
cd "$here/.."

pins="crates/gwk-pty/pins.env"
[[ -f "$pins" ]] || {
  echo "zig-bootstrap: $pins is missing — refusing to guess a version" >&2
  exit 2
}
# shellcheck disable=SC1090
source <(grep -E '^[A-Z_0-9]+=' "$pins")

: "${ZIG_VERSION:?pins.env defines no ZIG_VERSION}"
: "${ZIG_SHA256_X86_64_LINUX:?pins.env defines no ZIG_SHA256_X86_64_LINUX}"

root="${ZIG_INSTALL_ROOT:-$HOME/.local/zig}"
# Upstream's own directory name inside the tarball. Keeping it means several
# pinned versions can coexist, which matters the first time a re-pin has to be
# tested against the version it replaces.
dir="zig-x86_64-linux-$ZIG_VERSION"
dest="$root/$dir"

if [[ "${1:-}" == "--env" ]]; then
  echo "export PATH=\"$dest:\$PATH\""
  exit 0
fi

# One platform, checked rather than assumed. A silent install of a linux
# tarball on a mac would fail later and further away.
case "$(uname -s)/$(uname -m)" in
  Linux/x86_64) ;;
  *)
    echo "zig-bootstrap: this script only knows x86_64 linux, and you are on $(uname -s)/$(uname -m)." >&2
    echo "  Install zig $ZIG_VERSION from https://ziglang.org/download/$ZIG_VERSION/ and put it on PATH." >&2
    echo "  Only the checksum is platform-specific; the pin is not." >&2
    exit 2
    ;;
esac

if [[ -x "$dest/zig" ]]; then
  have="$("$dest/zig" version)"
  if [[ "$have" == "$ZIG_VERSION" ]]; then
    echo "zig-bootstrap: zig $ZIG_VERSION already at $dest"
    echo "export PATH=\"$dest:\$PATH\""
    exit 0
  fi
  echo "zig-bootstrap: $dest holds zig $have, not $ZIG_VERSION — replacing it" >&2
  rm -rf "$dest"
fi

tarball="$dir.tar.xz"
url="https://ziglang.org/download/$ZIG_VERSION/$tarball"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

echo "zig-bootstrap: fetching $url"
curl -fsSL --proto '=https' --tlsv1.2 -o "$tmp/$tarball" "$url"

# Verify BEFORE unpacking. An archive is a program's worth of filenames, and
# checking a tree after it has already been written to disk checks the wrong
# thing.
echo "$ZIG_SHA256_X86_64_LINUX  $tmp/$tarball" | sha256sum --check --status || {
  echo "zig-bootstrap: checksum mismatch for $tarball" >&2
  echo "  expected $ZIG_SHA256_X86_64_LINUX" >&2
  echo "  got      $(sha256sum < "$tmp/$tarball" | cut -d' ' -f1)" >&2
  echo "  Not unpacking. Either pins.env is stale or the download is not what it claims." >&2
  exit 1
}

mkdir -p "$root"
tar -xJf "$tmp/$tarball" -C "$root"

[[ -x "$dest/zig" ]] || {
  echo "zig-bootstrap: expected $dest/zig after unpacking, and it is not there" >&2
  exit 1
}
got="$("$dest/zig" version)"
[[ "$got" == "$ZIG_VERSION" ]] || {
  echo "zig-bootstrap: unpacked zig reports $got, expected $ZIG_VERSION" >&2
  exit 1
}

echo "zig-bootstrap: zig $ZIG_VERSION installed at $dest"
echo
echo "Add it to PATH, then materialize the ghostty tree:"
echo "  eval \"\$($0 --env)\""
echo "  ./tools/pty-toolchain.sh"
echo "  eval \"\$(./tools/pty-toolchain.sh --env)\""
echo "  cargo test -p gwk-pty"
