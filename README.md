# GridWork

**An agent operating system for the terminal.** One Rust binary, one append-only event
log as the source of truth, and a TUI as the only surface.

[![ci](https://github.com/GridWork-dev/gridwork/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/GridWork-dev/gridwork/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/gwk-domain.svg)](https://crates.io/crates/gwk-domain)
[![docs.rs](https://img.shields.io/docsrs/gwk-domain)](https://docs.rs/gwk-domain)
[![license](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

[gridwork.sh](https://gridwork.sh)

> **Pre-1.0 — expect breakage.** This project is being built in the open from its first
> commit. Schemas, protocols, and the binary itself change without notice until 1.0.
> Don't run `main` anywhere you care about.

## What this is

Coding agents are multiplying faster than the tools that supervise them. Run more than
one and you lose the things a single terminal used to give you for free: one place that
says what needs you, one record of what happened, and one answer to whether an agent was
allowed to do that. GridWork is an operating layer for a fleet of terminal agents:

- **One log.** Every platform truth — tasks, messages, gates, budgets, telemetry — is a
  projection of a single append-only event log. No dashboard database, no second truth.
- **A kernel, not a wrapper.** A daemon owns the event store, the attention queue, the
  authority policy (what agents may do unattended, what pages a human), workflow runs,
  and worktree lifecycle. Clients are thin.
- **Terminal-native.** The surface is a TUI: an orchestration mode — ruled as five
  lenses over one estate: hall, work, fleet, flow, term — and a workspace mode (a real
  multiplexer). No web console, ever.
- **Engine-agnostic.** Agents are driven over [ACP](https://agentclientprotocol.com) —
  an open protocol for talking to coding agents — plus engine hooks and PTY. Control
  never rides keystrokes.

## Where it actually is

Pre-alpha, at stage 5 of 6. **Stages 1–4 — the contract, the kernel, the
engines, and the console — are done**: the first two published on crates.io, stage 3
exiting with its twelve-cell parity matrix certified green at pinned engine versions
([docs/PARITY.md](docs/PARITY.md)). The kernel is a daemon owning an append-only event store,
projections written in the same transaction as their events, content-addressed
encrypted blobs, authority evaluation that leaves a receipt, event subscriptions, and
`gw` — the headless CLI over the same protocol the TUI uses. Certified against a
real PostgreSQL 16 and its performance envelope measured, not asserted.

Stage 3's crates are in this tree, deliberately unpublished until 1.0 packaging:
`gwk-pty` (a real server-side VT engine), the three `gwk-adapter-*` crates, the
parity harness that certifies them, and `gwk-pty-host` (the resident host:
session supervision, wire-frame conversion, command origination, and a
consumer-facing attach route — the host publishes each session's snapshots and
deltas into the kernel's session registry, and `PtyAttach`/`PtySnapshot` answer
consumers from there; a consumer's input now reaches the hosted session —
authority-gated, one ledger receipt per send — while routing resize and stop is
still open, and sessions still start from the operator's declaration
rather than a request). Stage 4 — the console — shipped as the ruled five-lens
workspace: `gw tui` opens HALL, WORK, FLEET, FLOW, and TERM over the kernel's
live projections and events, `gw board`, `gw event tail`, and `gw term attach`
open the same console shell focused on a lens, and every CLI verb renders a
real table on a TTY while a pipe still gets the wire JSON byte for byte. The
terminal-native bullet above now describes what runs, with one named gap:
persisted terminal recordings replay as a deterministic timeline in the
console, but the ledger-synced, exportable-as-evidence half of replay is not
built yet. Stage 5 — the workspace multiplexer — is the work now.

The build order — contract → kernel → engines → console → workspace → context
runtime — with what each
stage delivers, is in [ROADMAP.md](ROADMAP.md). For per-boundary status, the threat
model labels every stance *in force*, *partial*, or *designed, not yet built*.

## What you can run today

Two things. The first needs nothing but a Rust toolchain:

```bash
git clone https://github.com/GridWork-dev/gridwork
cd gridwork
cargo run -p gwk-cert -- crates/gwk-cert/fixtures/valid-stream.json
```

```
[]
gwk-cert: certified — 16 events, 0 findings
```

`gwk-cert` replays an exported event stream against the contract: envelope structure,
sequence monotonicity, state-machine edge legality, version discipline, terminal
immutability, and payload bounds. Findings go to stdout as typed JSON so CI can parse
them; the human summary goes to stderr. Exit `0` certified, `1` findings, `2` usage.

It takes one argument — a file path. There are no flags, so `--help` is read as a
filename.

**Explicit non-claim:** this proves *contract conformance*, not authenticity. A
coherent forged stream passes. Tamper evidence belongs to the storage layer, not to
stream inspection.

`cargo install gwk-cert` installs the same checker without a clone. The sample stream
ships inside the published crate too, under `fixtures/` in the unpacked source.

The second is the kernel. It needs a PostgreSQL 16, an EMPTY database it can own, and
**two separate roles** — that separation is load-bearing rather than tidy, so the
commands below are split the same way:

```bash
createuser --login gridwork                         # init GRANTS to this role; it never creates one
createdb --owner postgres gridwork

export GWK_BLOB_ROOT=$HOME/.local/share/gridwork/blobs
export GWK_BLOB_KEK=$(openssl rand -base64 32) GWK_BLOB_KEK_ID=dev
export GWK_SOCKET_PATH=$XDG_RUNTIME_DIR/gridwork/gwk.sock
export GWK_PUBLIC_REVISION=$(git rev-parse HEAD)
mkdir -m 700 -p "$(dirname "$GWK_SOCKET_PATH")"     # a group- or world-reachable one is refused

# One shot, and the only command that ever sees the schema-owner credential.
GWK_ADMIN_DATABASE_URL=postgres://postgres@localhost/gridwork \
GWK_RUNTIME_ROLE=gridwork \
  cargo run -p gridwork -- admin init               # schema, grants, genesis — all or nothing

# The daemon connects as the RUNTIME role. It refuses to start if it can see the
# admin credential in its environment, and refuses again if the credential it was
# given holds SUPERUSER, CREATEROLE, or UPDATE/DELETE on the log. Both are the
# point, not friction: the sole writer must not be able to rewrite history.
export GWK_DATABASE_URL=postgres://gridwork@localhost/gridwork
cargo run -p gridwork -- daemon &
cargo run -p gridwork -- kernel health   # {"type":"health","ready":true,"sealed":true}
```

A freshly initialized kernel is **sealed**: it answers questions, and refuses every
business command until `gw kernel activate` records the cutover. That boundary is
irreversible, so a quickstart deliberately stops short of it. `gw --help` lists the rest
of the surface; every answer is JSON on stdout, and the exit codes are stable (`0` ok,
`2` usage, `3` refused, `4` not found, `5` unavailable, `6` does not verify, `10` a fault
in `gw`).

## Built by the thing it builds

GridWork is not a cold start. It is the open rebuild of an internal agent OS that has
been running a real software operation for months: spec-driven phases, authority gates
deciding what agents may do unattended, event-sourced telemetry, and a fleet of coding
agents (Claude Code, Codex, opencode) shipping production systems under it.

This profile's contribution graph is the receipt — **7,300+ contributions in the five
months to July 2026, nearly all agent-authored, and until this repo, all of it in private repos**. This is the first
public one, and the agents that produced that graph are writing this codebase too:
most commits here are agent-authored under human direction and review. That's
disclosed as a fact, not a caveat — the same gates apply regardless of who typed the
code.

## Crates

| Crate | What | Status |
|---|---|---|
| [`gwk-domain`](https://docs.rs/gwk-domain) | Shared types, events, state machines — the contract | 0.0.2 |
| [`gwk-cert`](https://docs.rs/gwk-cert) | Stream checker, plus the storage suite a backend runs against its own event store | 0.0.2 |
| [`gwk-theme`](https://docs.rs/gwk-theme) | The 15 SIGNAL design tokens — one source for the site, the TUI, and the generated TypeScript | 0.0.2 |
| [`gwk-kernel`](https://docs.rs/gwk-kernel) | Daemon: event store, projections, blobs, attention, authority, the wire | 0.0.2 |
| [`gridwork`](https://docs.rs/gridwork) | Ships the `gw` binary — the CLI that speaks the kernel's protocol | 0.0.2 |
| [`gwk`](https://docs.rs/gwk) | Namespace root for the `gwk-*` crates. **No API** | 0.0.2, name only |
| `xtask` | Codegen and release glue. Not published | in-tree |
| `gwk-pty` | PTY engine: server-side VT, render-state deltas, reattach | in tree, unpublished |
| `gwk-pty-host` | Resident PTY engine host: session registry, spawn, detach/reattach routing | in tree, unpublished — not part of `cargo install gridwork` until its own release |
| `gwk-adapter-*` | Per-engine ACP + hooks adapters | in tree, unpublished |
| `gwk-parity` | The engine parity matrix harness — runs locally against logged-in engines, never in CI | in tree, unpublished |
| `gwk-text` | Pure column arithmetic and extended grapheme boundaries | in tree, unpublished |
| `gwk-tui` | The client: modes, lenses, palette | in tree, unpublished — five lenses and the live estate runtime; publish-flagged, ships with a future `gridwork` release |

`gwk` is published deliberately as a **name reservation with no API** — a module doc
block pointing at the crates that do the work. It is not a library and is not padded
into looking like one.

`cargo install gridwork` started working at 0.0.2. Through 0.0.1 that command got you
nothing, because the `gw` binary and the `gwk-kernel` behind it were in this tree and
not on crates.io; publishing the kernel is what changed, exactly as the 0.0.1 README
said it would. What you get is the headless CLI — it still needs a PostgreSQL 16 and
the two roles above before it does anything. Through the Console stage, that command
stays scoped to the kernel, the CLI, and the TUI's lenses: the PTY engine host is a
separate, unpublished process the installed binary never links (`gwk-tui` never
depends on `gwk-pty`, asserted in CI) and does not ship until packaging catches up to
it, at 1.0.

Library crates are prefixed `gwk-` (the crates.io name `gw` belongs to an unrelated
tool). **The binary is `gw`** — from the first build, permanently.

## Where to start reading

The contract first, then the one place that enforces it:

1. [`crates/gwk-domain/src/fsm.rs`](crates/gwk-domain/src/fsm.rs) — the four state
   machines. Each is an enum plus a fixed `EDGES` table, and the table *is* the
   contract: terminality is derived from it, so states and their legal moves cannot
   drift apart.
2. [`crates/gwk-domain/src/transition.rs`](crates/gwk-domain/src/transition.rs) — the
   one transition function every writer goes through. Pure, returns what happened as a
   value, never panics.
3. [`schema/0001_contract.sql`](schema/0001_contract.sql) — the same guarantees as
   database constraints. CI applies it to a pinned PostgreSQL and then attacks it:
   truncating a state table must fail, and clearing a set lease fence must fail.
4. [`crates/gwk-kernel/src/store.rs`](crates/gwk-kernel/src/store.rs) — the writer. One
   row locked for the length of an append, which is what makes sequence order equal
   commit order; the sequence is allocated from that row rather than a `BIGSERIAL`, so a
   rolled-back append gives its number back.
5. [`crates/gwk-kernel/src/recover.rs`](crates/gwk-kernel/src/recover.rs) — what a
   restart is allowed to *claim*. A checkpoint is evidence, not a restore point, and
   after a crash recovery says "unverified" instead of implying a check it did not run.
6. [`docs/security/THREAT_MODEL.md`](docs/security/THREAT_MODEL.md) — the honest
   per-boundary status of everything above.

## Docs

| | |
|---|---|
| [ROADMAP.md](ROADMAP.md) | The six stages and what each delivers |
| [docs/architecture.md](docs/architecture.md) | What owns truth, what the pieces are, which decisions are locked |
| [docs/protocol.md](docs/protocol.md) | The client↔kernel contract — framing, hello, requests, subscriptions |
| [docs/operations.md](docs/operations.md) | Running the daemon: startup and fencing, recovery, shutdown, KEK rotation |
| [docs/PARITY.md](docs/PARITY.md) | The engine parity matrix — four axes, per-engine acceptance tests, pinned versions |
| [docs/contract/NAMING.md](docs/contract/NAMING.md) | The casing and wire-shape rules the contract is frozen under |
| [docs/security/THREAT_MODEL.md](docs/security/THREAT_MODEL.md) | What GridWork defends against, and what it deliberately does not |
| [CLEANROOM.md](CLEANROOM.md) | The independent-implementation policy for terminal-engine work |
| [CONTRIBUTING.md](CONTRIBUTING.md) | Gates, prerequisites, and the traps worth knowing first |
| [SECURITY.md](SECURITY.md) | Reporting a vulnerability |

## Building

Stable Rust, MSRV 1.94. This builds the contract crates, the kernel, and the `gw` binary
— including `gw tui`, the early console surface. It is still not a finished product:
the stage-5 workspace (the real multiplexer) does not exist yet, and the engine host
remains a separate, unpublished process (see [ROADMAP.md](ROADMAP.md)).

```bash
cargo build
```

That is the default-member set, deliberately not `--workspace`: `--workspace` overrides
`default-members` and pulls in `gwk-pty`, which needs Zig (the exact version is pinned in
`crates/gwk-pty/pins.env`) and a ghostty source tree — and whose build script clones
ghostty when `GHOSTTY_SOURCE_DIR` is unset. Building the engine crates is
`eval "$(./tools/pty-toolchain.sh --env)"` first; CI covers them in their own `pty` and
`pty-host` jobs the same way.

The Rust half of the gate CI enforces:

```bash
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
cargo test --doc --locked
cargo +1.94 check --all-targets --locked
cargo deny check bans licenses sources
```

`cargo deny check` on its own also runs `advisories`, which CI deliberately keeps
non-blocking so a newly published advisory can't redden an unchanged PR.

The kernel's own suites need a live PostgreSQL and are `#[ignore]`d so that a clone
without one still passes. With a server, they are the ones worth running:

```bash
export GWK_TEST_ADMIN_DATABASE_URL=postgres://postgres@localhost:5432/postgres
cargo test -p gwk-kernel --locked -- --ignored \
  --skip the_phase_performance_envelope_holds        # crash, race and wire cases
cargo test -p gridwork --locked -- --ignored         # a real daemon on a temp socket
cargo test --release -p gwk-kernel --test perf -- --ignored --nocapture
```

Each case creates and initializes its **own** database — the log is append-only, so
there is no truncate-and-reuse path and sharing one would make an ordering-critical
suite order-dependent. The last line is the performance envelope; it refuses to run in a
debug profile, spends about two minutes measuring, and writes a receipt naming every
bound beside the method that produced it.

CI also checks the generated TypeScript, the SQL DDL, the site image, macOS, and two
publication gates. [CONTRIBUTING.md](CONTRIBUTING.md) has the rest of the list, the
tools you need installed, and the two gates most likely to surprise you.

## License

[Apache-2.0](LICENSE). Contributions are accepted under the same license
(inbound = outbound); there is no CLA and no DCO. See
[CONTRIBUTING.md](CONTRIBUTING.md).
