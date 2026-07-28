# Architecture

The implementation contract behind the roadmap: what owns truth, what the
pieces are, and which decisions are locked versus deliberately deferred.
Locked decisions change only by a recorded amendment, not by drift.

> **Read this as a design contract, not a description of running code.** It is
> written in the present tense throughout because it specifies what the pieces
> must do — but the kernel, the socket, projections, blobs, the PTY engine, the
> adapters, and the TUI are today all zero lines of code. What exists
> today is the contract itself: `gwk-domain`, `gwk-cert`, `gwk-theme`, the SQL
> DDL, and the generated TypeScript. `docs/security/THREAT_MODEL.md` labels each
> stance **in force** / **partial** / **designed, not yet built**; when this file
> and that one disagree about status, that one is right.

## Truth ownership

The event log owns **operational history** — not every byte in the system.

| Truth | Owner |
| --- | --- |
| Tasks, attempts, messages, commands, gates, receipts, budgets, telemetry | the append-only event log (`gwk.event`) — everything else is a projection |
| Current-state views, queues, boards, watermarks | projections — derived, rebuildable from the log, never hand-edited |
| Large payloads (transcripts, diffs, recordings) | content-addressed blobs, referenced from events by digest (`payload_ref`) |
| Working trees, repositories | git — the log records *references* (SHAs, branches, lease state), never file contents |
| Ephemeral runtime observation (liveness, load) | TTL'd observed state — explicitly OUTSIDE the log and the FSMs |

One consequence is non-negotiable: there is no second operational database.
A view that cannot be rebuilt from the log is a bug.

## Topology

**One artifact.** `gw` is a single binary with three modes: the kernel daemon
(owns the store and every write), the CLI (headless verbs over the same
protocol), and the TUI (the only human surface — there is no web console).
Clients are thin; policy, state machines, and authority live behind the
kernel boundary.

**Transport.** Local clients connect over a Unix domain socket. Remote use is
SSH to the host, then the same socket — the kernel does not listen on a
network interface, and will not until a recorded authentication decision
introduces one deliberately. Filesystem permissions on the socket are the
local trust boundary, reinforced by a same-EUID peer check. See
`docs/decisions/0001-wire-codec.md` and
`docs/decisions/0002-listener-before-auth.md`.

**Singleton fencing.** One kernel writes per store. Fencing tokens (strictly
increasing, invalidated on re-grant) make a deposed writer's appends fail
loudly instead of corrupting order.

## The append actor

`global_sequence` is assigned by a **dedicated append actor at commit time**,
in commit order. It is unique and strictly increasing, and explicitly NOT
gapless. Database serial/identity columns were rejected as an ordering proof:
they allocate at insert time while transactions commit in another order, so a
reader paging an allocation-ordered column can observe N+1 before N commits
and then never see N. Assigning at commit closes the hole by construction —
`schema/0001_contract.sql` encodes this, and the storage conformance suite
(`gwk-cert::conformance`) certifies it for any backend.

## The storage ports

Storage is engine-neutral behind two traits in `gwk-domain::port`. `EventStore`
is the log: atomic append with an expected-version CAS, read-by-cursor,
watermark, and fencing. `BlobStore` is the out-of-line content spine:
streaming upload, read, stat, pin/unpin, sweep, and crypto-shred over
plaintext-addressed blobs. The first production backend is PostgreSQL; an **embedded backend is
a real release phase, not an aspiration** — the port and its conformance
suite exist so that claim is testable, and engine-specific mechanics
(queues, notification channels, lock strategies) are confined to backend and
deployment layers, never contract semantics. For PostgreSQL that boundary is
literal: `gwk-kernel` owns the driver, and everything it needs beyond the
contract lives in a separate `gwk_internal` schema, so which schema an object
sits in tells you whether it is contract or mechanism.

## Projections and watermarks

Every consumer holds a durable cursor (`global_sequence`). Notifications are
an optimization, never load-bearing: a consumer that sleeps through every
wakeup recovers the complete suffix by re-reading from its cursor. Rebuild is
deterministic — replaying the same log yields the same projection, byte for
byte.

## Payloads and blobs

Events carry bounded inline JSON metadata (64 KiB serialized). Anything
larger lives outside the log as an **encrypted, content-addressed blob**
referenced by digest, media type, and size. Blobs carry a retention class;
deletion is by retention sweep or crypto-shred (destroying the key), and an
`evidence_pin` exempts a blob from sweeps while it backs an audit trail. The
log itself never shrinks. The persisted format and key hierarchy are locked by
`docs/decisions/0003-payload-encryption.md`.

## Crash recovery

The kernel checkpoints its coordination state (open attempts, held leases,
pending approvals, budget cursor) as data. Recovery is: load the checkpoint,
re-read the log from its cursor, reconcile against live observation — and
trust the log over the checkpoint wherever they disagree. Attempts whose real
outcome cannot be established terminate as `unknown`, never a fabricated
`failed` or `succeeded`.

## Contract surfaces

The Rust crate `gwk-domain` is canonical. Three derived surfaces are checked,
not trusted: the generated TypeScript (`contracts/bindings.ts`, with golden
round-trip fixtures decoded at runtime in CI), the SQL DDL
(`schema/0001_contract.sql`, applied clean against the pinned
PostgreSQL major in CI), and the kernel's embedded copy of that DDL
(`crates/gwk-kernel/src/contract_sql.rs`, which a published crate needs because
`include_str!` cannot reach outside its own package). `gwk-cert` certifies
event streams against the same tables the types are built from. Naming rules:
`docs/contract/NAMING.md`.

## Platforms

Linux and macOS are first-class targets. Windows is supported via WSL2 on a
best-effort basis; a native Windows port is out of scope pre-1.0.
