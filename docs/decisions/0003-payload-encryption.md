# ADR 0003: Chunked XChaCha20-Poly1305 payload encryption

- Status: accepted
- Date: 2026-07-28

## Context

Content-addressed blobs can contain transcripts, diffs, and evidence. Plaintext
content-addressing enables deduplication but cannot make sensitive retained payloads safe
to discard by deleting only metadata. Encryption must support bounded streaming,
tamper detection, KEK rotation, and crypto-shred without rewriting blob ciphertext.

## Decision

The address is `sha256:<64 lowercase hex>` over plaintext. Each blob receives a random
32-byte DEK and is encrypted in 1 MiB plaintext chunks with XChaCha20-Poly1305 streaming
AEAD plus an explicit authenticated final chunk.

The KEK is exactly 32 bytes supplied as base64 outside persisted payloads; its nonsecret
identifier is persisted. A separate random nonce wraps the DEK. Wrap AAD binds format
version, plaintext digest, media type, byte size, and KEK ID.

The binary container has a fixed magic value, explicit format version, authenticated
strict header, and length-prefixed ciphertext chunks. Writes use a same-filesystem
temporary file, file fsync, atomic rename, and parent-directory fsync. Caller paths are
never accepted; the validated digest alone determines the sharded destination.

Deduplication additionally requires equal size and media type. Rewrap changes only the
wrapped DEK. Crypto-shred first commits a tombstone and removes the wrapped DEK; ciphertext
cleanup follows asynchronously, and tombstoned reads always fail.

## Consequences

KEK rotation does not rewrite large blobs, interrupted writes are distinguishable from
committed blobs, and authenticated truncation is detectable. Losing the KEK or deleting a
DEK is intentionally irreversible.
