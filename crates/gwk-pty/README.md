# gwk-pty

The PTY engine: bytes from a child process in, an authoritative terminal grid out.

**This crate needs a non-Rust toolchain.** It is the only one in the workspace that does,
which is why it is not a default workspace member — `cargo test` at the repo root does not
build it, and does not need Zig.

## Building it

```bash
./tools/pty-toolchain.sh                    # pinned ghostty tree + Zig package graph
eval "$(./tools/pty-toolchain.sh --env)"    # GHOSTTY_SOURCE_DIR + GHOSTTY_ZIG_SYSTEM_DIR
cargo test -p gwk-pty
```

Zig itself is not installed by that script — get the exact version in
[`pins.env`](pins.env) from ziglang.org. Distro packages are already past it, and
0.15 → 0.16 was a breaking language change, so "I have Zig" is usually the wrong Zig.

## Why two environment variables

`GHOSTTY_SOURCE_DIR` stops `libghostty-vt-sys` cloning ghostty during the build.
`GHOSTTY_ZIG_SYSTEM_DIR` passes `zig build --system`, which stops **Zig** resolving
ghostty's own package graph over the network. Setting only the first still gives you a
build that reaches the network — it just does it one layer down, where you will not see it.

A caveat that costs disk: `--system` resolves the package graph eagerly, so the script
fetches with `--fetch=all` and pulls every dependency ghostty declares, including ones
libghostty-vt does not use. That is ~565 MB of Zig packages against a 134 MB source tree.
Both are gitignored and CI caches them keyed on the pinned revision.

## Pins

All three pins in [`pins.env`](pins.env) move together. `libghostty-vt` ships FFI bindings
pre-generated from `ghostty/vt.h` at a specific revision, so bumping ghostty without
bumping the crate gives you bindings that compile and then lie about the struct layout
underneath them. Re-pinning is budgeted at one per phase.

## No `unsafe` here

The VT implementation is consumed through libghostty-vt's **safe** wrapper, not the raw
`-sys` bindings — the workspace forbids `unsafe_code`, so raw FFI would not compile in this
tree at all. The honest consequence: the FFI boundary is real, it is just not ours to
audit. libghostty-vt is pre-1.0 and says so itself.
