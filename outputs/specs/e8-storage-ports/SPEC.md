---
phase: e8-storage-ports
project: gridwork
slug: e8-storage-ports
title: E8 — Context storage ports and CAS metadata
status: accepted
tags: [security, secrets, data-migration]
created: 2026-09-01
pr: 172
source: the 8′ context-runtime program plan, Workstream 8B ("Provenance spine"), Task 7 — amended at EXECUTE 2026-08-28; this file records the phase in the repository it changes
---

# SPEC — E8: Context storage ports and CAS metadata

> Context blobs ride the shipped encrypted content-addressed store unchanged and gain
> classification: three closed class axes (content, redaction, retention), enforced as
> Rust enums, as contract DDL `CHECK`s, and by a token-parity gate holding the two
> together. This SPEC is the phase document for PR #172. The program plan it derives
> from lives outside this public repository; everything a reader needs to judge the
> change is restated here.

## Goal

Give the kernel a typed, engine-neutral way to store and read Context blobs by
digest, bounded by content class, so that later tasks (the deterministic compiler and
the verifier) consume classified evidence through ports instead of reaching into the
blob store — without changing a single byte of the container format, the wire
contract, or the running kernel's behaviour.

## Why

- **Provenance needs classification, not a new store.** Prompt, body, and derived
  Context bytes carry different redaction and retention obligations. Those obligations
  have to be recorded beside the blob, closed-set and machine-checked, or every consumer
  re-derives them by convention.
- **One key per content class (R19).** A single key over every class means one
  compromised consumer reads everything. Per-class KEK identities make a cross-class
  read a typed refusal, and a mis-keyed ring an authentication failure — never a
  silent mis-decrypt.
- **Retention must be data, not code (R20).** Another hardcoded branch in the SQL macro
  is exactly the kind of drift that has already forced contract regeneration twice; a
  retention class column with a closed `CHECK` set is auditable from the DDL alone.
- **Reuse over extension (R21).** `gwk.evidence` and `blob_pin` already model
  evidence linkage and pinning. Context blobs become new evidence kinds; nothing new is
  invented until a projection empirically cannot answer.

## Scope

In scope:

- Port traits — content-addressed put/get bounded by class, evidence linkage,
  projection reads — carrying no GridWork policy values. The CAS port and the class
  vocabulary land in `gwk-domain::context` beside `port.rs` (amended home, see
  Constraints); `crates/gwk-context/src/store.rs` carries the typed truth-record READ
  port its consumers need.
- A kernel-side adapter over the existing evidence/blob primitives
  (`crates/gwk-kernel/src/blob/context.rs`, `blob/store.rs`), plus the class → KEK
  configuration (`crates/gwk-kernel/src/config.rs`).
- Contract DDL: a digest-keyed Context blob-metadata table (`gwk.context_blob`) with
  content, redaction, and retention class columns, registered as a contract step
  (`schema/steps/7ebb2ada-be73d920.sql`), joining the REVOKE list, the grant-class
  map, `EXPECTED_RELATIONS`, and both guard-count equalities in the same commit.
- The retention sweep's one new data-driven arm: configured `(class, days)` windows,
  fail-safe for a class with no window, `permanent` structurally without a window,
  and the classified-digest carve-out from the ordinary-evidence-forever rule.
- Conformance tests against a postgres-gated adapter, and a mutation battery proving
  every guard can fail.

Out of scope:

- Any change to the container's AEAD, truncation detection, or on-disk format (R17).
- Wiring the ports into the V1 dispatch path — Task 32 owns consumer readiness, E20
  owns activation.
- Any wire-contract bump: `CONTRACT_VERSION == 1` and the three V2-refusal sites stay
  unchanged.
- A `gwk-kernel → gwk-context` dependency (see Constraints; Task 10 inherits the bind).

## Requirements

| Id | Requirement | Where it is enforced |
|---|---|---|
| R17 | Container bytes untouched; classification is metadata beside the blob. The header's nonsecret `kek_id` already exists; per-class keys are new KEK identities, not a format change. | adapter regression canary through the existing container tests |
| R19 | One KEK per content class; the class → KEK-id mapping is typed and closed; keys come from the environment (`GWK_CONTEXT_KEK_*`, one key plus nonsecret label per class); a missing class key fails closed at process start, never at first use; distinct labels enforced. | `ContextBlobConfig` construction; conformance suite (cross-class read = typed refusal; swapped-KEK ring = AEAD authentication failure) |
| R20 | Retention is a first-class class column with a closed `CHECK` set. The classification row is append-only and outlives the bytes as the retention audit's record. | DDL `CHECK`; token-parity gate between the Rust enum and the DDL set; sweep arm tests |
| R21 | `gwk.evidence` and `blob_pin` reused as-is; pins ride the existing (digest, evidence id) set and override expiry unconditionally. | sweep tests in both directions (pinned-expired survives, unpinned-expired reclaimed) |
| DDL policy | The new table joins the REVOKE list and the grant-matrix append-only arm in the same commit; the step joins the migrate suite's real-database arm. | grant-script unit test; migrate suite two-step chain proof |

## Acceptance criteria

Behaviour this phase adds, each captured RED before the adapter existed:

1. Every read is digest-addressed.
2. A blob sealed under one class KEK refuses to open under another — as a typed
   metadata refusal for a cross-class read, and as an authentication failure for a
   mis-keyed ring.
3. A retention class outside the closed set is refused at `INSERT`.
4. Pinned evidence survives retention expiry; unpinned expired evidence is reclaimed
   by the very next sweep.
5. No table row carries reconstructable prompt or body bytes.
6. Bytes already sealed under a non-class key domain (a content collision with a
   kernel-internal blob) are refused before any classification is claimed.

Mutation proof (green baseline committed first; each restored via checkout + touch):

- drop the pin arm from the sweep → the retention test reds with the pinned blob in
  the swept set;
- remove the classified-digest carve-out → reds the other way (ordinary evidence
  protects an expired blob forever);
- remove the whole class-window arm → reds with four blobs swept where one should be;
- remove the retention `CHECK` from the deployed DDL → the closed-set test reds at
  exactly that axis, after its positive control landed;
- drop `gwk.context_blob` from the REVOKE list → the grant-script unit test reds,
  printing the shrunken statement;
- the swapped-KEK and fail-closed-key-ring arms are live tests, not one-off edits.

Contract machinery: the DDL digest moves, so the registered step rides the same commit
(no backend migration); embedded copies regenerate; `bindings.ts` and the goldens are
byte-identical (no new TypeScript root). The migrate suite proves the two-step chain
end-to-end: base reconstruction from history, both steps applied, one ledger row per
step, `pg_dump` equality against a fresh initialization.

## Constraints

- **Amended home for the ports (EXECUTE, 2026-08-28).** `gwk-kernel` is published and
  `cargo package` structurally refuses a dependency on an unpublished crate — the
  bind the `package` job's own xtask comment records, with both ways out closed. So
  the CAS port and class vocabulary live in `gwk-domain`, not `gwk-context`. Task 10
  inherits the same bind for any future `gwk-kernel → gwk-context` line.
- The running kernel stays on wire V1 throughout; nothing here activates.
- Secrets never enter the repository: the per-class keys are environment-supplied and
  the only committed artefacts are their nonsecret labels and variable names.

## Tags

- `security` — a new trust boundary (class-bounded reads, per-class key domains).
- `secrets` — new environment-supplied key material with fail-closed loading.
- `data-migration` — contract DDL adds a table and a registered migration step.

These tags fire the security and migration-safety audits at SHIP; the receipts are
published as `ship/security` and `ship/migration` commit statuses on the head that
merges, alongside `ship/review`.

## Verify

Offline suites across `gwk-domain`, `gwk-context`, `gwk-kernel`, `xtask`;
postgres-gated suites (`admin_init`, `admin_migrate`, `blob_store`, `context_store`
plus the full ignored set — the perf harness's debug-mode refusal is release-only by
design); `cargo clippy` at zero warnings per crate; `cargo fmt` clean; `contract
--check` clean; check-claims clean; the cleanroom gate clean on the staged path list;
`CONTRACT_VERSION == 1`; known V2 still refused at all three sites.
