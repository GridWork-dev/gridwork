#!/usr/bin/env bash
# Run a command with no network, and refuse to run it at all if that cannot be
# arranged. Used to prove gwk-pty builds without reaching out (5a′ ruling 2: the
# PTY build performs no build-time network access).
#
# WHY NOT `cargo --offline`. That flag gates CARGO's registry access. It says
# nothing about what a build script does, and the two things being ruled out here
# — libghostty-vt-sys cloning ghostty, and Zig resolving a package graph — are
# neither of them cargo. A job using only `--offline` goes green while proving
# something adjacent to the claim.
#
# The sandbox is a network namespace with no interfaces but loopback. Two ways
# in, because they are available in different places:
#
#   unshare -rn      unprivileged user namespace. Works on an ordinary Linux
#                    desktop; GitHub-hosted runners refuse it with
#                    "write failed /proc/self/uid_map: Operation not permitted".
#   sudo unshare -n  needs passwordless sudo, which hosted runners have and a dev
#                    box generally does not. `setpriv` drops straight back to the
#                    calling user, so nothing builds as root and no cache ends up
#                    root-owned.
#
# THE ENVIRONMENT HAS TO BE CARRIED ACROSS BY HAND on the sudo path, and getting
# that wrong is how this script failed the first time it ran on a hosted runner:
# sudo's env_reset drops exported variables and its secure_path replaces PATH, so
# the build died with "cargo: command not found" inside a sandbox that was
# otherwise working correctly. `sudo -E` is not the fix — a plain
# `NOPASSWD: ALL` sudoers line does not grant SETENV, so -E can be refused
# outright. `runuser -p` is not the fix either; it rewrites PATH regardless.
# So the whole environment is handed over explicitly, NUL-separated to survive
# values containing spaces or newlines, and a canary proves it arrived.
#
# If no sandbox is available, this exits non-zero WITHOUT running the command. A
# proof that silently downgrades to "ran it with the network on" is worse than no
# proof: it reports success for a claim it never tested.
set -euo pipefail

if [[ $# -eq 0 ]]; then
  echo "usage: no-network.sh <command> [args...]" >&2
  exit 2
fi

mode=""
if unshare -rn true 2> /dev/null; then
  mode=userns
elif sudo -n unshare -n true 2> /dev/null; then
  mode=sudo
else
  cat >&2 <<'EOF'
no-network.sh: cannot create a network namespace.

Tried `unshare -rn` (unprivileged user namespaces) and `sudo -n unshare -n`.
Neither is available here, so the no-network claim cannot be tested — and this
script will not run the command with the network up and call that a pass.

On a dev box: enable unprivileged user namespaces, or run the build yourself and
treat the network-free property as unverified.
EOF
  exit 1
fi

sandboxed() {
  case "$mode" in
    userns) unshare -rn "$@" ;;
    sudo)
      local envs=()
      mapfile -d '' -t envs < <(env -0)
      sudo -n unshare -n setpriv \
        --reuid "$(id -u)" --regid "$(id -g)" --init-groups \
        -- env "${envs[@]}" "$@"
      ;;
  esac
}

# Self-check, because the failure this guards against is silent: a sandbox that
# starts but loses the environment produces "command not found" a hundred lines
# later, which reads as a broken build rather than a broken wrapper.
canary="$(NO_NETWORK_CANARY=ok sandboxed bash -c 'printf %s "${NO_NETWORK_CANARY:-}"' || true)"
if [[ "$canary" != ok ]]; then
  echo "no-network.sh: the $mode sandbox did not preserve the environment — refusing to run" >&2
  exit 1
fi
if sandboxed bash -c 'getent hosts github.com' > /dev/null 2>&1; then
  echo "no-network.sh: the $mode sandbox still resolves DNS — it is not isolating anything" >&2
  exit 1
fi

echo "no-network.sh: running under the $mode sandbox (no interfaces, env preserved)" >&2
sandboxed "$@"
