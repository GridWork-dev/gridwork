# Protocol

The client↔kernel contract. Semantics and byte-level framing are locked by
[ADR 0001](decisions/0001-wire-codec.md). The UDS-only authentication boundary
is locked by [ADR 0002](decisions/0002-listener-before-auth.md).

> **This is implemented.** The daemon, the socket, the framing, the handshake,
> the request surface and event subscriptions are all in the tree and certified
> against a real PostgreSQL — `gw` is a client of exactly what is described
> below. `cargo install gridwork` gets you the client; the daemon runs from the
> same binary (`gw daemon`, or `cargo run -p gridwork -- daemon` from a clone).

## Connection and hello

Clients connect to the kernel's Unix domain socket (remote: SSH to the host,
same socket — see `docs/architecture.md` for the transport stance). The first
exchange is a `hello`:

- the client sends its protocol version and requested capabilities,
- the kernel answers with its version, the capability set it grants, and the
  store's current watermark.

Version negotiation is strict: an unknown **major** version is a typed
refusal, never a best-effort session. Capabilities are additive names
(open-set, snake_case) — a client must not assume a capability it was not
granted in the hello.

## Framing and bounds

Messages carry a big-endian unsigned 32-bit body length, a one-byte frame kind, and the
body. The length includes the kind byte and excludes the prefix; it is
`1..=4,194,304`, leaving at most 4,194,303 payload bytes after the kind. A zero or
oversized `body_length`, and every unknown kind, is refused before body allocation.

Kind `0x01` is strict UTF-8 JSON control; invalid UTF-8, recursively duplicate-keyed,
unknown-field, and trailing-byte JSON inputs are refused. Capability-gated kind `0x02`
is one opaque PTY payload and may contain any bytes, including an empty payload. Its strict
JSON header immediately precedes it and pins the request, optional sequence, and exact byte
count. Publish headers name the session directly; delivery headers carry the attach request
and generation, with the session established by the preceding `pty_raw_attached` response.
A connection that was not granted `pty_raw` refuses that path. A publisher has five seconds
after a raw header to deliver its paired payload; expiry closes the connection and releases
its hosted sessions. Bounds are part of the contract:

- a frame has a hard maximum size (rejected, not truncated, when exceeded),
- inline event payloads are bounded at 64 KiB serialized — larger content
  travels as content-addressed blob references (`payload_ref`),
- every 64-bit counter is a canonical decimal **string** on the wire
  (`docs/contract/NAMING.md`), so no JSON consumer silently rounds it.

## PTY attach modes and backpressure

`pty_attach` is the primary render-state path: a styled snapshot plus bounded delta
batches. `pty_raw_attach` is the fallback: a model-produced VT snapshot followed by the
child's original output bytes in kind `0x02` frames; resizes remain typed JSON controls
because they are not byte-stream events. Both paths use the same session generation and
frame-revision cursor, so a reconnect either replays a retained gap or reseeds without
claiming continuity across one.

`pty_input_delivery` and `pty_control` negotiate generation-addressed reverse delivery;
`pty_start` identifies the resident host connection that receives starts. Input, resize,
and stop are authority-gated metadata commands, delivered only after commit through
the bounded owner-connection control queue; resize and stop name `{session_id, generation}`
so they cannot cross a reclaimed lifetime. These delivery-bearing commands are refused on
the generic `submit_command` request and must use their dedicated request variants, which is
where capability checks and post-commit host delivery run. A start carries a declared template
name and a session id only. The host re-reads the active `pty_session_template` projection for
program, arguments, cwd, `env:NAME` environment references, and initial geometry; the resident
host resolves those references at spawn time, and the child inherits no host environment beyond
that declared map. Environment values never enter the event log or projection. Arbitrary
executable data never crosses the start request.
The authority-gated request remains baseline JSON and does not negotiate the host-only
`pty_start` receiving role. A replacement start-manager connection supersedes the prior route,
and ended local sessions are reaped on a bounded resident cadence.

The event append creates a durable pending-delivery row in the same transaction. An attempt
takes a short database lease, releases the transaction before waiting on the host, and settles
the row from the connection-checked application acknowledgement. Failure before dispatch releases
the claim for retry; an explicit host refusal is terminal and does not reconnect the publisher
or block a later stop. A disconnect or expired/orphaned claim after dispatch is terminally
`indeterminate` because the host may already have applied the control; the kernel never recycles
that claim into an unsafe duplicate side effect. Applied, failed, and indeterminate replays return
their durable result without another control. The host keeps a bounded event-id dedup window for
the crash interval between applying a control and committing its acknowledgement. After durable
settlement the kernel sends `pty_delivery_settled`, which lets the host forget that event id. If
that frame is lost, a reconnect reasserts retained applied acknowledgements: delivered rows simply
confirm settlement, while indeterminate rows reconcile to delivered. A saturated unsettled window
refuses new applications rather than evicting an identity that could still retry.

All declared, published, and requested PTY geometries are bounded to 1,000 cells per axis and
100,000 cells total before resident grid allocation. The compact interned wire representation
does not relax that in-memory bound.

One raw seed is accepted per session generation. A session then retains at most 1,024 raw
events and 8 MiB; reaching either bound refuses the publisher, whose reconnect creates a
new generation and seed rather than growing the daemon. Each connection's outbound batch
queue holds eight items. A reader that leaves that queue full for 30 seconds loses only its
raw attach with `slow_consumer`; queued items for the closed stream are discarded, while
the hosted child and the primary render stream continue. If the reader instead blocks the
header/payload pair already being written for 30 seconds, the connection closes: a partial
frame cannot be followed by a recoverable typed close.

## Authentication

Locally, the socket's filesystem permissions are the boundary — a process
that can open the socket is the operator. Remotely, SSH provides transport
and identity. There are no bearer tokens on this surface today because there
is no network listener; introducing one requires a dedicated, recorded
authentication decision first (a deliberate choice, not an oversight).

## Subscriptions and reconnect-by-cursor

Reads are cursor-driven: a subscription names a `global_sequence` cursor and
receives ordered events after it. Delivery is at-least-once from the cursor;
consumers dedupe by `event_id` or sequence. Notifications are an
optimization — after any disconnect, sleep, or missed wakeup, re-subscribing
from the durable cursor recovers everything, in order, with nothing missing.
The watermark call answers "how far does the log go" without a subscription.

## Commands and idempotency

State changes are requested as commands (`CommandEnvelope`), never as direct
writes. Every command carries a required `idempotency_key`: reissuing the
same key is stable — the kernel answers with the original result rather than
applying twice. CAS is explicit: commands that target an aggregate carry
`expected_version`, and a stale expectation is a typed `stale_version`
refusal carrying the actual version, so retry logic re-reads instead of
guessing.

## Errors

Errors are values in the contract, not strings to parse:

- transitions answer `applied | illegal_edge | stale_version |
  unauthorized_actor` (`TransitionResult` in the bindings),
- appends refuse with version conflict, fencing, malformed batch, or opaque
  storage failure,
- an unknown envelope `schema_version` is a typed error unless an upcaster
  covers it — never a silent partial read.

## Reference

The envelope field reference and the four state machines' full edge tables
are generated from the canonical Rust source — read them in
`contracts/bindings.ts` (types), `schema/0001_contract.sql` (DDL + edge
seed), and `crates/gwk-domain/src/fsm.rs` (the tables themselves).
`gwk-cert` certifies any exported stream against them.
