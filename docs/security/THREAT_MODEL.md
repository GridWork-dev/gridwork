# Threat model

What GridWork defends against, what it deliberately does not, and where each
mitigation lives. Written for contributors: if your change touches one of
these boundaries, the relevant stance is review criteria, not background
reading. Reporting: see `SECURITY.md`.

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

### 2. Terminal escape injection (ANSI/OSC)

Malicious output can abuse escape sequences to spoof UI, alter the scrollback,
or (in vulnerable terminals) trigger worse.
**Stance:** raw agent/program bytes stay `[u8]` end to end internally; the
TUI renders through a virtual-terminal model rather than replaying raw bytes
to the host terminal, and anything echoed outside that model is
escape-stripped. Control never rides synthetic keystrokes, so output cannot
"type" into a session.

### 3. Hostile or buggy socket clients

Any local process with socket access can send arbitrary frames.
**Stance:** filesystem permissions bound WHO connects (see
`docs/protocol.md`); strict framing bounds and `deny_unknown_fields`
decoding bound WHAT they can say; commands are CAS-guarded, idempotent, and
policy-checked, so a misbehaving client can be refused but not corrupt
order. No network listener exists before a dedicated auth ADR.

### 4. Log tampering and projection poisoning

An attacker (or bug) rewriting history, or feeding a poisoned projection.
**Stance:** append-only is enforced at the contract level (triggers) and the
storage level (privilege hardening in deployment); the append actor + fencing
prevent write races; projections are rebuildable from the log, so a poisoned
cache is recoverable by rebuild. **Explicit non-claim:** `gwk-cert` certifies
internal consistency of a stream — it cannot detect a coherent FORGERY from
stream input alone. Tamper *evidence* is a storage/provenance property
(append-only enforcement, host controls), not a stream-inspection property.

### 5. Path traversal and symlink games

Worktree, blob, and evidence paths cross the kernel boundary.
**Stance:** kernel-side path resolution with containment asserts (resolve,
then require the result inside the allowed root); worktree identity is by id
and lease, not caller-supplied paths; blob access is by digest, never by
client-named file path.

### 6. Secret echo into transcripts and the log

Agents and tools print environment values; transcripts become blobs; the log
is long-lived.
**Stance:** inline payloads are bounded metadata — bulk output goes to blobs
with retention classes and crypto-shred deletion; a redaction pass gates
transcript capture; the public repository's history is leak-scanned in CI
with seeded proof the gate can fail. Secrets never appear in contract types.

### 7. Resource exhaustion

Unbounded frames, unbounded payloads, runaway agents.
**Stance:** hard frame and payload bounds at the protocol layer; per-attempt
budgets (tokens, tool calls, wall clock, cost) as contract data with
kill-and-alert semantics; bounded channels internally — backpressure over
buffering.

### 8. Provenance of agent-authored code

Most commits are agent-authored; a poisoned suggestion is a supply-chain
vector.
**Stance:** disclosed provenance (`AI-Assisted-By` trailer, non-authorship),
human direction and review on every merge, the same CI gates regardless of
author, and a clean-room policy (`CLEANROOM.md`) with independent second
review for terminal-engine-adjacent changes.

### 9. Agent-protocol downgrade

A hostile or broken agent endpoint negotiating weaker behavior (e.g. dropping
permission relays).
**Stance:** version negotiation is strict (unknown major = refusal, never
best-effort); capabilities are explicit grants in the hello; the stable wire
version is pinned per adapter and CI rejects unstable protocol features;
permission prompts relay through the kernel — an adapter that cannot relay
them does not get write capabilities.

## Non-goals

Defending the host from its own operator; sandboxing arbitrary code the
operator chooses to run (that is the OS/container layer's job); detecting
forged-but-coherent imported streams (see threat 4); multi-tenant isolation —
GridWork is a single-operator system pre-1.0.
