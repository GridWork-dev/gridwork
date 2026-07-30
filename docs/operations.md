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
