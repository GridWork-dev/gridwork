# Threat model

What GridWork defends against, what it deliberately does not, and where each
mitigation lives. Written for contributors: if your change touches one of
these boundaries, the relevant stance is review criteria, not background
reading. Reporting: see `SECURITY.md`.

GridWork is pre-alpha, stage 1 of 5 (see `ROADMAP.md`) — several stances below
are design commitments whose enforcing code does not exist yet. Each stance
therefore carries a status: **in force** (enforced at HEAD), **partial**
(naming what is enforced versus designed), or **designed, not yet built**. A
design stance is a commitment the build is held to, not a shipped mitigation.

## Assets

The event log (operational history + audit trail), payload blobs
(transcripts, diffs — often sensitive), the authority policy (what agents may
do unattended), credentials in the host environment, and the operator's
repositories and working trees.

## Trust boundaries

1. **Agent ↔ kernel.** Agents are credentialed but NOT trusted: they process
   untrusted content (web pages, issues, code) and can be prompt-injected.
   Everything an agent asks for crosses the kernel's policy boundary.
2. **Client ↔ kernel socket.** Local processes that can open the socket;
   remote users via SSH.
3. **Terminal output ↔ operator's terminal.** Bytes produced by agents and
   arbitrary programs are rendered in the operator's terminal.
4. **Stream import boundary.** Event streams and blobs can be exported and
   re-imported (backup, migration, certification).

## Threats and stances

### 1. Prompt-injected agents (confused deputy)

An agent reading attacker-controlled content can be steered to exfiltrate
data or take destructive actions with its own credentials.
**Stance:** agents never write platform state directly — every state change
is a command through the kernel, subject to authority policy (policy as data:
what runs unattended, what pages the operator). Dangerous action classes gate
on explicit grants; every automated decision leaves a receipt in the log.
Residual risk is real and disclosed: within its granted read surface, an
injected agent can still read and egress — containment bounds the blast
radius, it does not make injection impossible.

**Status: designed, not yet built.** The command-envelope shapes and the
authority-grant schema exist in the contract; there is no kernel yet, so no
policy evaluation runs and no receipts are emitted.

### 2. Terminal escape injection (ANSI/OSC)

Malicious output can abuse escape sequences to spoof UI, alter the scrollback,
or (in vulnerable terminals) trigger worse.
**Stance:** raw agent/program bytes stay `[u8]` end to end internally; the
TUI renders through a virtual-terminal model rather than replaying raw bytes
to the host terminal, and anything echoed outside that model is
escape-stripped. Control never rides synthetic keystrokes, so output cannot
"type" into a session.

**Status: designed, not yet built.** No terminal-handling code exists yet.

### 3. Hostile or buggy socket clients

Any local process with socket access can send arbitrary frames.
**Stance:** filesystem permissions bound WHO connects (see
`docs/protocol.md`); strict framing bounds and `deny_unknown_fields`
decoding bound WHAT they can say; commands are CAS-guarded, idempotent, and
policy-checked, so a misbehaving client can be refused but not corrupt
order. No network listener exists before a dedicated auth ADR.

**Status: partial.** Strict unknown-field rejection and the CAS + idempotency
command semantics are implemented in the contract crates; the framing bounds,
the filesystem-permission boundary, and the policy check are designed, not yet
built — there is no socket to connect to yet.

### 4. Log tampering and projection poisoning

An attacker (or bug) rewriting history, or feeding a poisoned projection.
**Stance:** append-only is enforced at the contract level (triggers) and the
storage level (privilege hardening in deployment); the append actor + fencing
prevent write races; projections are rebuildable from the log, so a poisoned
cache is recoverable by rebuild. **Explicit non-claim:** `gwk-cert` certifies
internal consistency of a stream — it cannot detect a coherent FORGERY from
stream input alone. Tamper *evidence* is a storage/provenance property
(append-only enforcement, host controls), not a stream-inspection property.

**Status: partial.** The append-only database triggers are in force —
including TRUNCATE coverage, guards on the insert and delete paths, and
enforcement that survives replica-mode sessions. Fencing exists as a contract
type and a column, not yet as enforced behavior; no projections exist yet to
rebuild. The `gwk-cert` non-claim describes the shipped certifier exactly.

### 5. Path traversal and symlink games

Worktree, blob, and evidence paths cross the kernel boundary.
**Stance:** kernel-side path resolution with containment asserts (resolve,
then require the result inside the allowed root); worktree identity is by id
and lease, not caller-supplied paths; blob access is by digest, never by
client-named file path.

**Status: designed, not yet built.** No code handles an untrusted path yet —
the stance is the rule that code will be built to.

### 6. Secret echo into transcripts and the log

Agents and tools print environment values; transcripts become blobs; the log
is long-lived.
**Stance:** inline payloads are bounded metadata — bulk output goes to blobs
with retention classes and crypto-shred deletion; a redaction pass gates
transcript capture; the public repository's history is leak-scanned in CI
with seeded proof the gate can fail. Secrets never appear in contract types.

**Status: partial.** The CI leak scan (with its seeded proof of failure) and
the inline payload byte bound — a contract constant the certifier checks — are
in force. The blob store, retention classes, crypto-shred deletion, and the
redaction pass are designed, not yet built.

### 7. Resource exhaustion

Unbounded frames, unbounded payloads, runaway agents.
**Stance:** hard frame and payload bounds at the protocol layer; per-attempt
budgets (tokens, tool calls, wall clock, cost) as contract data with
kill-and-alert semantics; bounded channels internally — backpressure over
buffering.

**Status: partial.** The inline payload maximum is in force, and budgets are
real as contract data; the frame bounds, kill-and-alert enforcement, and
bounded channels are designed, not yet built.

### 8. Provenance of agent-authored code

Most commits are agent-authored; a poisoned suggestion is a supply-chain
vector.
**Stance:** disclosed provenance (`AI-Assisted-By` trailer, non-authorship),
human direction and review on every merge, the same CI gates regardless of
author, and a clean-room policy (`CLEANROOM.md`) with independent second
review for terminal-engine-adjacent changes.

**Status: in force.**

### 9. Agent-protocol downgrade

A hostile or broken agent endpoint negotiating weaker behavior (e.g. dropping
permission relays).
**Stance:** version negotiation is strict (unknown major = refusal, never
best-effort); capabilities are explicit grants in the hello; the stable wire
version is pinned per adapter; permission prompts relay through the kernel —
an adapter that cannot relay them does not get write capabilities.

**Status: partial.** Strict refusal of an unknown envelope `schema_version`
is implemented in the contract types; the hello negotiation, capability
grants, and permission relay are designed, not yet built.

## Non-goals

Defending the host from its own operator; sandboxing arbitrary code the
operator chooses to run (that is the OS/container layer's job); detecting
forged-but-coherent imported streams (see threat 4); multi-tenant isolation —
GridWork is a single-operator system pre-1.0.
