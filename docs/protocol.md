# Protocol

The client↔kernel contract. **Semantics below are locked at the contract
phase; the exact byte-level frame encoding is finalized with the kernel and
captured as a recorded decision** — clients written against the semantics
will not need redesign, only a codec.

> **Nothing here is implemented.** There is no kernel, no socket, and no
> framing code in the tree — write a client against these semantics if you want
> to be ready, but you cannot connect to anything today.

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

Messages are length-prefixed frames carrying one JSON value, encoded per the
generated contract (`contracts/bindings.ts` mirrors the canonical Rust
types). Bounds are part of the contract:

- a frame has a hard maximum size (rejected, not truncated, when exceeded),
- inline event payloads are bounded at 64 KiB serialized — larger content
  travels as content-addressed blob references (`payload_ref`),
- every 64-bit counter is a canonical decimal **string** on the wire
  (`docs/contract/NAMING.md`), so no JSON consumer silently rounds it.

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
