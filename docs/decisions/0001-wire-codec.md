# ADR 0001: Versioned length-prefixed wire codec

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
`1..=4,194,304`. A zero or oversized length and every unknown kind are typed refusals.
Kind `0x01` carries exactly one UTF-8 JSON control value; invalid UTF-8, trailing bytes,
duplicate keys at any nesting depth, unknown fields, noncanonical decimal strings, and
unsupported major versions are refused on that path. Kind `0x02` was reserved for the
later terminal engine and is claimed by the additive capability below.

The terminal-engine reservation is now claimed additively: a hello that grants
`pty_raw` permits kind `0x02`, whose body is one opaque terminal-byte payload. A strict
JSON header immediately precedes every raw payload and carries its correlation, optional
snapshot sequence, and exact byte count. Publish headers name the session; delivery headers
resolve it through the preceding attach response. Peers that did not negotiate `pty_raw`
continue to use kind `0x01` only, so the reservation became an optional surface without
changing the major grammar. A raw publish payload must follow its header within five seconds;
an incomplete pair closes the connection because its next frame boundary is unresolved.

The first control frame is `hello`, is at most 64 KiB, and must arrive within five
seconds; no other request is decoded before negotiation succeeds. The hello carries the
major/minor pair; request-scoped and asynchronous controls carry request IDs. Wire `u64`
values are canonical decimal strings. JSON objects use generated strict schemas and tagged
unions; clients never infer a variant from field presence.

Blob bytes do not appear in ordinary JSON frames. Blob transfer uses bounded control
frames whose chunks are separately length-checked and correlated to a negotiated request.

## Consequences

The codec is simple to inspect and implement across Rust and Bun. JSON remains human
diagnosable, while recursive duplicate-key rejection requires a validating decoder rather
than direct permissive deserialization. Any incompatible encoding change requires a new
major protocol version and ADR.
