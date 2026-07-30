#!/usr/bin/env bash
# Run a command with no network, and refuse to run it at all if that cannot be
# arranged. Used to prove gwk-pty builds without reaching out (CLEANROOM-adjacent
# ruling: the PTY build performs no build-time network access).
#
# WHY NOT `cargo --offline`. That flag gates CARGO's registry access. It says
# nothing about what a build script does, and the two things being ruled out here
# — libghostty-vt-sys cloning ghostty, and Zig resolving a package graph — are
# neither of them cargo. A job using only `--offline` goes green while proving
# something adjacent to the claim.
#
# The sandbox is a network namespace with no interfaces but loopback. Two ways in,
# because they are available in different places:
#
#   unshare -rn        unprivileged user namespace. Works on a normal Linux
#                      desktop; GitHub-hosted runners refuse it with
#                      "write failed /proc/self/uid_map: Operation not permitted".
#   sudo unshare -n    needs passwordless sudo, which hosted runners have and a
#                      dev box generally does not. Drops straight back to the
#                      calling user so nothing in the build runs as root and the
#                      cache does not end up root-owned.
#
# If neither works, this exits non-zero WITHOUT running the command. A proof that
# silently downgrades to "ran it with the network on" is worse than no proof: it
# reports success for a claim it never tested.
set -euo pipefail

if [[ $# -eq 0 ]]; then
  echo "usage: no-network.sh <command> [args...]" >&2
  exit 2
fi

if unshare -rn true 2> /dev/null; then
  exec unshare -rn "$@"
fi

if sudo -n unshare -n true 2> /dev/null; then
  # `-p` keeps the environment, so HOME still points at the calling user's cargo
  # and rustup directories rather than root's.
  exec sudo -n unshare -n runuser -u "$(id -un)" -p -- "$@"
fi

cat >&2 <<'EOF'
no-network.sh: cannot create a network namespace.

Tried `unshare -rn` (unprivileged user namespaces) and `sudo -n unshare -n`.
Neither is available here, so the no-network claim cannot be tested — and this
script will not run the command with the network up and call that a pass.

On a dev box: enable unprivileged user namespaces, or run the build yourself and
treat the network-free property as unverified.
EOF
exit 1
