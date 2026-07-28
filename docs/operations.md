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
