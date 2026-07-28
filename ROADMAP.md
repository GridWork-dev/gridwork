# Roadmap

GridWork is built in public, pre-1.0. Stages land in order, and each stage exits with
working, tested code — there are no aspirational branches.

No dates. A stage is done when its gates are green, not when a calendar says so.

## 1 · Contract *(shipped)*

The shared language everything else speaks. Published to crates.io at 0.0.1 under
Apache-2.0, all five crates with live docs.rs pages.

- Domain types, event schemas, and four state machines in `gwk-domain`. Each machine is
  an enum plus a fixed edge table, and the table *is* the contract: terminality is
  derived from it, so a state and its legal moves cannot drift apart. The invariant
  suite walks those tables exhaustively and a mutation battery proves the suite
  actually catches a tampered edge
- One pure transition function every writer goes through. It returns what happened as a
  value — applied, illegal edge, stale version, unauthorized actor — and never panics
- A storage port with a conformance suite any backend runs against its own event store,
  plus `gwk-cert`: a checker that replays an exported event stream against the contract
- A generated TypeScript contract for non-Rust consumers, CI-checked against the
  committed artifact, with a golden round trip that decodes in Bun and re-verifies in
  Rust by value
- A SQL DDL that CI applies to a pinned PostgreSQL and then attacks: truncating a state
  table must fail, and clearing a set lease fence must fail

## 2 · Kernel *(current)*

In flight — nothing merged yet.

- A daemon owning an append-only event store — the sole writer; every other view is a
  projection
- The attention queue: one prioritized feed of everything that needs a human
- Authority policy as data: what agents may do unattended, what always pages
- Headless CLI verbs over the same protocol the TUI will use

## 3 · Engines

- PTY engine: authoritative server-side virtual terminal, render-state deltas,
  detach/reattach, recording
- Agent adapters for Claude Code, Codex, and opencode over ACP + engine hooks —
  control never rides synthetic keystrokes
- A parity matrix per engine, including permission-prompt relay

## 4 · Console

- The orchestration TUI: Queue (attention), Board (work), and a live view of the
  running fleet
- Time-synced replay of ledger + terminal, exportable as evidence

## 5 · Workspace

- A full terminal multiplexer: workspaces, tabs, splits, scrollback, detach
- Daily-driver quality — the stage where GridWork becomes the terminal you live in

## 1.0

All pillars complete and packaged: single-command install (`cargo install gridwork`
and friends), and distribution stops assuming you run your own postgres — a zero-setup
embedded backend is part of 1.0 packaging, not before.

## Principles that won't move

- One append-only log owns every truth; user interfaces are projections of it.
- The kernel is the sole writer; clients are thin.
- Control never rides keystrokes.
- Terminal only. No web console.
