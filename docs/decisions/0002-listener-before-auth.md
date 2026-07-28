# ADR 0002: No listener before an authentication decision

- Status: accepted
- Date: 2026-07-28

## Context

A TCP or HTTP listener changes the kernel from a same-host, single-operator boundary into
a network service. Adding the listener first and authentication later creates an
unauthenticated interval and makes transport availability drive security design.

## Decision

The v1 kernel exposes only a Unix domain socket. It creates the socket under an
operator-owned mode-0700 runtime directory, sets the socket to mode 0600, verifies the
peer effective UID, and accepts only the daemon's effective UID. Filesystem ownership and
the same-EUID check are both mandatory.

The kernel contains no TCP, HTTP, WebSocket, QUIC, or abstract-namespace listener and no
Bearer-token path. Remote operation uses SSH to the host and then the same UDS.

Any future network listener requires a new accepted ADR defining authentication,
authorization, transport confidentiality, replay protection, revocation, rate limiting,
and deployment exposure before listener code lands.

## Consequences

There is one local trust boundary and no dormant network attack surface. Socket forwarding
and multi-user access remain out of scope for v1.
