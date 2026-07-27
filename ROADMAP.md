# Roadmap

GridWork is built in public, pre-1.0. Stages land in order, and each stage exits with
working, tested code — there are no aspirational branches.

## 1 · Contract *(current)*

The shared language everything else speaks.

- Domain types, event schemas, and state machines in `gwk-domain`, with property tests
- Event-stream conformance checking in `gwk-cert`
- A generated TypeScript contract for non-Rust consumers, CI-checked against the
  committed artifact

## 2 · Kernel

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

All pillars complete and packaged. The binary takes its final name (`gw`), and
distribution stops assuming you run your own postgres — a zero-setup embedded backend
is part of 1.0 packaging, not before.

## Principles that won't move

- One append-only log owns every truth; user interfaces are projections of it.
- The kernel is the sole writer; clients are thin.
- Control never rides keystrokes.
- Terminal only. No web console.
