# Kernel operations contract

Kernel v1 is a same-EUID Unix-domain-socket daemon backed by PostgreSQL. The normative
transport, authentication, and blob-format decisions are ADRs
[0001](decisions/0001-wire-codec.md), [0002](decisions/0002-listener-before-auth.md), and
[0003](decisions/0003-payload-encryption.md).

Initialization uses a schema-owner connection that is never passed to the daemon. Runtime
receives only its least-privilege database role and blob KEK. It refuses superuser, role
creation, DDL, and direct history-mutation privileges.

Startup acquires a nonblocking PostgreSQL advisory lock on a dedicated monitored
connection, increments the durable writer epoch under row lock, recovers projections, then
creates the UDS. Loss of the lock connection cancels acceptance and fences stale work.

The runtime directory is mode 0700 and the socket is mode 0600. Stale sockets are removed
only after ownership, file-type, and failed-connect checks. Readiness is emitted only after
recovery, fencing, privilege checks, and socket setup succeed.

Notifications wake readers but never establish truth. Durable cursor reads recover every
gap. Shutdown stops acceptance, drains bounded work for at most 30 seconds, checkpoints,
removes the owned socket, and exits.

The parting checkpoint is taken after the drain, under the writer barrier, at the
watermark — which is what lets the next start report `verified` instead of `unverified`.
It cannot block the exit: a snapshot that fails is reported on the `daemon_stopped` line
as `checkpoint_error` and the socket comes off regardless, because the alternative is
leaving the next kernel to inherit a socket and a writer lock. Watch that field. A
barrier that has silently stopped firing grows recovery time without bound, and the first
symptom is otherwise a restart that replays the entire log.

## Rotating the blob KEK

Every blob's data key is wrapped under the KEK and stored in its row, never in its
container ([ADR 0003](decisions/0003-payload-encryption.md)). Rotation therefore rewrites
32 bytes per blob and no ciphertext at all, however large the store is.

The label does not change. `GWK_BLOB_KEK_ID` is copied into each container's
authenticated header, so relabeling would invalidate the very AAD the new wrap is bound
to. Rotation replaces the key behind the name; a store holding two labels at once is not
a state this supports.

```
GWK_BLOB_KEK=<the running key> GWK_BLOB_KEK_NEXT=<the new key> gw admin blob rotate
```

Both keys are base64 over exactly 32 bytes, and both arrive in the environment: a KEK on
a command line is a KEK in the shell history and in every `ps` on the box. The answer
carries two counts — `rewrapped`, what this run moved, and `already_rotated`, what an
earlier run had already moved.

**The daemon is not part of this and does not learn.** It holds the old key for as long
as it is running, so from the moment `rotate` succeeds until the daemon is restarted with
the new key in `GWK_BLOB_KEK`, every blob read fails. The sequence is rotate, swap the
variable, restart, verify one read — and the window between the first and third steps is
a blob-read outage, so plan it as one.

Re-running is safe. One row per statement means an interruption lands between blobs,
leaving a prefix on the new key and the rest on the old, both carrying the same label,
with nothing on the row recording which is which. A second run works that out per blob
and finishes the job: `rewrapped: 0` beside a nonzero `already_rotated` is a completed
rotation, not a rotation that found nothing to do. A blob neither key opens is reported
as an integrity failure and stops the run rather than being counted as done.

Shredded blobs are skipped throughout. There is no key left to rewrap, and minting one
would be un-shredding.
