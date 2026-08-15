# ADR 0004: The Context wire is a second major, named before it is served

- Status: accepted
- Date: 2026-08-14

## Context

The Context plane adds commands, reads, and lifecycle events to the client↔kernel wire.
None of them fit the v1 grammar, and a plane that records what an agent was told cannot be
optional: a manifest that some peers write and others skip explains nothing. So Context
arrives as a version change rather than a capability. The grammar has to exist and be
reviewable before any code speaks it, because a grammar whose first implementation lands in
the same change is a grammar whose first implementation defines it.

## Decision

Protocol major `2` is added to the version grammar and served by nobody. `ProtocolVersion`
knows `V1` and `V2`; the kernel accepts `V1` alone. A v2 hello therefore decodes cleanly,
reaches the explicit major check, and is refused with `unsupported_version`. A major outside
the grammar — `3` and up — still fails inside `Deserialize` and surfaces as `validation`.
The two codes are distinct on purpose: one means the version does not exist, the other means
it exists and this kernel does not speak it, and a client deciding between upgrading and
giving up needs to tell them apart.

Refusal is stated on both ends. Naming a second major turns "an ack at an unexpected major
cannot decode" into "an ack at an unexpected major decodes and something must look at it", so
the `gridwork` client and the PTY host's kernel client each check the acknowledged major and
refuse a mismatch. A kernel that refuses a v2 client is half a negotiation; a client that
accepts whatever it is answered with is the downgrade that threat 9 in the threat model
exists to prevent.

Framing is unchanged. `[u32 body_length][u8 frame_kind][body]` and the `1..=4,194,304` bound
from ADR 0001 carry v2 exactly as they carry v1. This is a grammar change, not a codec
change, and nothing about the byte layout is reopened.

The two version axes stay separate. `ProtocolVersion` is the wire grammar; `CONTRACT_VERSION`
is the domain contract — entity, event, and command shapes — and it remains `1` through this
change and through the eventual cutover. It moves when a shape moves, on its own merits. A
reflexive bump alongside the wire major would teach every later reader that the two travel
together, which is exactly the belief that makes the next contract change unreviewable.

Nothing Context-specific rides the hello. The handshake stays major, minor, capabilities, and
client; there is no `context_capable` field and no `context` capability name. Mandatory and
capability are opposites — at major 2 every participant speaks Context, and there is no
optional v1 Context mode, no translator, no proxy, and no dual stack. Hello is also
per-connection while Context resolution is per-attempt, so a connection-scoped flag would be
answering at the wrong lifetime even if the grammar wanted one.

Attribution on a Context lifecycle event is provenance, never authorization. Same-EUID
remains the entire authentication boundary for the kernel socket, unchanged from threat 10;
Context events inherit it verbatim and nothing here narrows it. What this grammar adds is
narrower and structural: a client cannot supply source attribution. The command shape carries
the fact and nothing else, and the compiler derives attribution by re-reading its own
resolved manifest rather than by trusting a caller's claim about itself.

Serving major 2 is a separate, operator-gated act — element E20 of the Context Runtime
phase, which owns the cutover and is not this decision. Until it happens, the types published here
are shapes with no handlers, and adding one is an ordinary contract change. After it happens,
rollback is bounded by what the log already holds: recorded Context events do not disappear
when the accepted major moves back, so reverting the cutover restores the served version and
not the state. Any change that makes the kernel accept major 2 requires a new accepted ADR
recording the activation and its rollback limit before the code lands.

## Consequences

Two majors now exist in one enum, so every reader of a version number must ask which
question it is answering — known, or served — and the answer is no longer the same. That is
the cost, and it buys a refusal a client can act on plus a full phase in which the Context
grammar can be reviewed, regenerated into TypeScript, and argued with while nothing depends
on it. The published shapes are cheap to change today and expensive the moment a peer speaks
them; the cutover is the point at which that flips, which is why it is gated rather than
inferred from the grammar being ready.
