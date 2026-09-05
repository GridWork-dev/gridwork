---
name: rust-workflow
description: Run GridWork's default Rust checks and cleanroom gate without pulling excluded toolchain crates.
---

# Rust workflow

Use this for ordinary Rust validation in this repository.

- Run bare `cargo clippy` and `cargo test`. **“Never `--workspace` or
  `--all-features` on cargo.”** `CLAUDE.md:44-45` says six crates are outside
  `default-members` for toolchain reasons; either flag overrides that boundary
  and pulls in the Zig-dependent lane.
- For changed cleanroom paths, use the repository's exact gate pipe:

  ```bash
  git diff --cached --no-renames --name-only | ./tools/cleanroom-gate.sh
  ```

- Commits assisted by a tool carry `AI-Assisted-By: <tool>` as required by
  `CLAUDE.md`.
