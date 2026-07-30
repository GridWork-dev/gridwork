# ADR 0001: Versioned length-prefixed JSON wire codec

- Status: accepted
- Date: 2026-07-28

## Context

The kernel needs a deterministic local protocol that generated Rust and TypeScript
clients can decode strictly. It must reject ambiguous JSON, bound allocation before
parsing, preserve `u64` values exactly, and leave room for additive capabilities without
silently weakening a major-version boundary.

## Decision

Protocol v1 uses one unsigned big-endian 32-bit body length, a one-byte frame kind, then
the frame body. The length includes the kind byte and excludes the prefix; it is
`1..=4,194,304`. Kind `0x01` carries exactly one UTF-8 JSON control value. Kind `0x02` is
reserved for the later terminal engine and is neither advertised nor accepted in v1. A
zero or oversized length, unknown kind, invalid UTF-8, trailing bytes, duplicate key at
any nesting depth, unknown field, noncanonical decimal string, or unsupported major
version is a typed refusal.

The first control frame is `hello`, is at most 64 KiB, and must arrive within five
seconds; no other request is decoded before negotiation succeeds.
All envelopes carry `protocol_version = 1` and a request ID. Wire `u64` values are
canonical decimal strings. JSON objects use generated strict schemas and tagged unions;
clients never infer a variant from field presence.

Blob bytes do not appear in ordinary JSON frames. Blob transfer uses bounded control
frames whose chunks are separately length-checked and correlated to a negotiated request.

## Consequences

The codec is simple to inspect and implement across Rust and Bun. JSON remains human
diagnosable, while recursive duplicate-key rejection requires a validating decoder rather
than direct permissive deserialization. Any incompatible encoding change requires a new
major protocol version and ADR.
