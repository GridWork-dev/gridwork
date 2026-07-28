# ADR 0001: Versioned length-prefixed JSON wire codec

- Status: accepted
- Date: 2026-07-28

## Context

The kernel needs a deterministic local protocol that generated Rust and TypeScript
clients can decode strictly. It must reject ambiguous JSON, bound allocation before
parsing, preserve `u64` values exactly, and leave room for additive capabilities without
silently weakening a major-version boundary.

## Decision

Protocol v1 uses one unsigned big-endian 32-bit payload length followed by exactly one
UTF-8 JSON value. The maximum payload length is 1 MiB. A zero-length frame, oversized
length, invalid UTF-8, trailing bytes, duplicate key at any nesting depth, unknown field,
noncanonical decimal string, or unsupported major version is a typed refusal.

The first request is `hello`; no other request is decoded before negotiation succeeds.
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
