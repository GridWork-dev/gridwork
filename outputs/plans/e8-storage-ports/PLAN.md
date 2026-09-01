---
phase: e8-storage-ports
project: gridwork
slug: e8-storage-ports
title: E8 — Context storage ports and CAS metadata
status: executed
spec: outputs/specs/e8-storage-ports/SPEC.md
created: 2026-09-01
pr: 172
---

# PLAN — E8: Context storage ports and CAS metadata

Route: RUST. One atomic implementation commit (`feat(context): typed storage ports
over the kernel CAS`, PR #172) followed by this phase-doc commit. The plan is recorded
as executed: every task below names the file set it landed in and the check that
proves it.

## Tasks

### Task 1 — Port traits and the class vocabulary

Files: `crates/gwk-domain/src/context/` (CAS port beside `port.rs`; content,
redaction, and retention class enums, closed), `crates/gwk-domain/src/lib.rs`;
`crates/gwk-context/src/store.rs` (typed truth-record READ port).

Public traits carry no GridWork policy values. A backend proves itself by conformance,
not by being first.

Verify: `cargo test -p gwk-domain --locked`, `cargo test -p gwk-context --locked`.

### Task 2 — Kernel adapter and the per-class key ring

Files: `crates/gwk-kernel/src/blob/context.rs` (new), `crates/gwk-kernel/src/blob.rs`,
`crates/gwk-kernel/src/blob/store.rs`, `crates/gwk-kernel/src/config.rs`
(`ContextBlobConfig`: one key plus nonsecret label per class from `GWK_CONTEXT_KEK_*`,
all-or-nothing at construction, distinct labels enforced), `crates/gwk-kernel/src/admin.rs`.

R17 holds by construction: the adapter composes the existing container; the class
metadata row is written beside the blob.

Verify: `cargo test -p gwk-kernel --locked --test context_store` (postgres-gated),
`cargo test -p gwk-kernel --locked --test blob_store`.

### Task 3 — Contract DDL and the registered step

Files: `schema/0001_contract.sql` (`gwk.context_blob`, digest-keyed, three class
columns with closed `CHECK` sets), `schema/steps/7ebb2ada-be73d920.sql` (registered
step; no backend migration), `xtask/src/contract.rs`, `xtask/src/steps.rs`,
`crates/gwk-kernel/src/migrate.rs`, `crates/gwk-kernel/tests/admin_migrate.rs`,
`crates/gwk-kernel/tests/admin_init.rs`.

Slice DDL policy in the same commit: the table joins the REVOKE list, the grant-class
map, `EXPECTED_RELATIONS`, and both guard-count equalities (TRUNCATE 19, row-level
DELETE 20). The step-chain gate's expired bootstrap anchor is deleted on its own
documented instruction — an empty registry is now always a refusal.

Verify: `cargo run -p xtask -- contract --check`; `cargo test -p gwk-kernel --locked
--test admin_migrate` (two-step chain: base reconstruction, both steps, one ledger row
per step, `pg_dump` equality against a fresh initialization); `cargo test -p xtask`.

### Task 4 — Retention as data, pins reused

Files: the sweep in `crates/gwk-kernel/src/blob/store.rs`; configuration in
`crates/gwk-kernel/src/config.rs`.

One data-driven arm: `(class, days)` windows arrive as parallel arrays; a class with no
window is retained (fail safe); `permanent` has no window variable; a classified digest
is carved out of the ordinary-evidence-forever rule. Pins ride the existing
(digest, evidence id) set and override expiry unconditionally.

Verify: the `context_store` suite's retention arms in both directions.

### Task 5 — Conformance RED arms and the mutation battery

Files: `crates/gwk-kernel/tests/context_store.rs`.

RED captured before the adapter existed for the six acceptance criteria in the SPEC.
Mutation battery run against a committed green baseline, each mutation restored via
checkout + touch; the swapped-KEK and fail-closed-key-ring arms stay as live tests.

Verify: the battery's six reds as listed in the SPEC, then the full green run:
offline suites across the four crates, postgres-gated suites, clippy at zero warnings,
fmt clean, check-claims clean, cleanroom gate clean.

### Task 6 — Phase docs and SHIP receipts

Files: `outputs/specs/e8-storage-ports/SPEC.md`, `outputs/plans/e8-storage-ports/PLAN.md`.

Tracking note: this repository ignores `/outputs/` (local session artefacts hold
private paths and must not publish). These two files are the first tracked phase docs;
they were added with `git add -f` and are scanned by the leak gate like any other
tracked file. A future phase doc needs the same force-add, or a narrower ignore rule.

SHIP: the SPEC's tags fire the security and migration-safety audits. Their receipts are
published as `ship/security`, `ship/migration`, and `ship/review` commit statuses on
the exact head that merges — after the branch is re-synced with `main` (strict
required checks), and only once, so the branch's CI concurrency group is never
cancelled mid-run.

Verify: `git diff --name-only origin/main...HEAD -- 'outputs/specs/*/SPEC.md'
'outputs/plans/*/PLAN.md'` prints both paths; `./tools/leak-scan.sh` clean; the three
`ship/*` statuses read `success` on the PR head via the commit-status API.
