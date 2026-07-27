# GridWork

**An agent operating system for the terminal.** One Rust binary, one append-only event
log as the source of truth, and a TUI as the only surface.

> **Pre-1.0 — expect breakage.** This project is being built in the open from its first
> commit. Schemas, protocols, and the binary itself change without notice until 1.0.
> Don't run `main` anywhere you care about.

## Built by the thing it builds

GridWork is not a cold start. It is the open rebuild of an internal agent OS that has
been running a real software operation for months: spec-driven phases, authority gates
deciding what agents may do unattended, event-sourced telemetry, and a fleet of coding
agents (Claude Code, Codex, opencode) shipping production systems under it.

This profile's contribution graph is the receipt — **7,300+ contributions in five
months, nearly all agent-authored, all of it in private repos**. This is the first
public one, and the agents that produced that graph are writing this codebase too:
most commits here are agent-authored under human direction and review. That's
disclosed as a fact, not a caveat — the same gates apply regardless of who typed the
code.

## What this is

Coding agents are multiplying faster than the tools that supervise them. GridWork is an
operating layer for a fleet of terminal agents:

- **One log.** Every platform truth — tasks, messages, gates, budgets, telemetry — is a
  projection of a single append-only event log. No dashboard database, no second truth.
- **A kernel, not a wrapper.** A daemon owns the event store, the attention queue, the
  authority policy (what agents may do unattended, what pages a human), workflow runs,
  and worktree lifecycle. Clients are thin.
- **Terminal-native.** The surface is a TUI: an orchestration mode (attention queue,
  work board, a live view of the fleet) and a workspace mode (a real multiplexer). No
  web console, ever.
- **Engine-agnostic.** Agents are driven over [ACP](https://agentclientprotocol.com)
  plus engine hooks and PTY — control never rides keystrokes.

## Status and roadmap

Pre-alpha, stage 1 of 5. The build order — contract → kernel → engines → console →
workspace — with what each stage delivers, lives in [ROADMAP.md](ROADMAP.md).

## Crates

| Crate | What | Status |
|---|---|---|
| `gwk-domain` | Shared types, events, state machines — the contract | skeleton |
| `gwk-cert` | Contract conformance checker for event streams | skeleton |
| `xtask` | Codegen + release glue | skeleton |
| `gwk-kernel` | Daemon: event store, projections, attention, authority | planned |
| `gwk-pty` | PTY engine: server-side VT, render-state deltas, reattach | planned |
| `gwk-adapter-*` | Per-engine ACP + hooks adapters | planned |
| `gwk-tui` | The client: modes, lenses, palette | planned |
| `gwk-cli` | Headless verb twin of the TUI | planned |

Crates are prefixed `gwk-` (the crates.io name `gw` is taken). The installed binary is
`gwk` during pre-1.0 development and ships as `gw` at 1.0.

## Building

Stable Rust, MSRV 1.88.

```bash
cargo build --workspace
```

The quality gate that CI enforces (all green before any PR):

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo test --workspace --doc --locked
cargo deny check
```

## License

[Apache-2.0](LICENSE). Contributions are accepted under the same license
(inbound = outbound); there is no CLA and no DCO. See
[CONTRIBUTING.md](CONTRIBUTING.md).
