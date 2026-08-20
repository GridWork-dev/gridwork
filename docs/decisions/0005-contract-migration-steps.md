# ADR 0005: A contract change is a step, and a step has to prove itself

- Status: accepted
- Date: 2026-08-19

## Context

`gwk_internal.schema_fingerprint` holds one row: the SHA-256 of the
`schema/0001_contract.sql` a database was initialized from. A binary refuses to serve a
database whose digest is not its own, which is the right refusal and, until now, the only
one available — there was no way for a database to move from one digest to another. A
contract change reached every new database and no existing one.

One database exists. It was initialized at `aba2f647…` and the contract has moved three
times since: `44d20e0` (#103), `e680b00` (#112), `b77e78b` (#147). The gap was not
theoretical.

## Decision

Six choices, each with what it costs.

**A1 — steps live in the repository.** `schema/steps/<base8>-<result8>.sql`, each declaring
its base and result digests in a header, authored in the PR that changes the contract and
reviewed with it. The alternative was a diff the verb computes at apply time from a schema
differ nobody here has written, reviewed, or seeded with a failing case — an unreviewable
apply against a production log. **Cost:** one more authored file per contract change, and a
CI gate that refuses a moved digest with no step resulting in it. That gate is the price
and the point: it makes forgetting the step impossible rather than unlikely.

**B2 — refuse by default, `--from <sha256>` asserts a different base.** The SPEC
recommended B1 (always refuse) and B2 was taken because a database whose fingerprint does
not describe its actual schema is a real state with no other exit. **Cost, and what the
override owes:** this is the one path where the verb applies a chain whose precondition it
has not checked. So both the receipt and the ledger row carry `asserted_base: true`
alongside the digest actually recorded at the time — that row is the only thing that will
ever say the check was skipped. The override relaxes the assertion, not the chain: a
`--from` naming a digest no step bases on is still refused.

**C3 — the scratch proof runs beside the live database**, on the same server and therefore
the same major by construction, via the `beside()` helper `rebuild-projections` already
uses. The alternative was a throwaway container, which owns the `pg_dump` 17 /
postgres 16 `SET transaction_timeout` trap permanently and makes the verb unrunnable on a
host without docker — missing on the day it is needed. **Cost:** the scratch consumes disk
proportional to the live database on the production host. **Not implemented — see Known
gaps.**

**D3 — `--backup <path>` is required, and the verb never produces the backup.** It opens
the file the operator names, computes its SHA-256 itself, and records what it computed
rather than what it was told. **Cost:** the major-version judgement stays with the
operator, who is the only party who knows which `pg_dump` matches the server. `--no-backup`
exists, has to be typed, and names the restore path it removes.

**E2 — the writer lock is taken before the pool, as `init` takes it.** It never waits, so a
live daemon is a refusal rather than a hang. **Cost, and why both orderings fail safe:** the
lock going first means a typo'd backup path could fence a running kernel for nothing, so the
backup is read and digested *before* the lock — the check that costs nothing runs before the
one that takes something away. Either ordering is safe; only one of them is kind.

**F2 — an append-only `gwk_internal.schema_migration` ledger**, one row per applied step,
with `SELECT` only for the runtime role. **Cost, and the trap it steps around:** the ledger
lives in `gwk_internal`, where `backend_script`'s blanket `GRANT … ON ALL TABLES IN SCHEMA
gwk` does not reach, so it has to be named explicitly. The blanket grant is exactly how
#147's Context records arrived UPDATE-able — the same mistake on the one table whose job is
to be evidence. Append-only is enforced by TRIGGER rather than by the withheld grant: a
grant binds the runtime role and nothing else, and the credential that applies a migration
is the admin one.

### Consequences recorded as decisions, not discovered later

**Intermediate digests are unmigratable by construction.** The retroactive step collapses
three contract moves into one `aba2f647… → 7ebb2ada…`. A database sitting at `7d80f97…`
(post-#103) or at the digest #112 produced has no chain and cannot be migrated. Accepted
while exactly one database exists. If a second ever sits at an intermediate digest, the
answer is to author the missing steps, not to widen the resolver — the resolver refusing is
the mechanism working.

**A migrated database and a freshly initialized one differ in column ordinal position.**
`gwk.gate.decided_by` and `gwk.pty_session.engine_session_id` sit last on a migrated
database, because PostgreSQL appends an added column and the contract declares both
mid-table. Both databases report the same contract digest, and that is correct: the digest
identifies the contract the schema conforms to, not the physical layout it happens to have.
No query in this codebase reads a column by position. Closing the gap would mean rebuilding
both tables and recreating the CAS and append-only triggers on `pty_session` — trading a
guard that might not come back for a difference nothing can observe. Asserted rather than
removed, so a *third* difference can never hide behind it.

## Known gaps

**`gwk.dispatch_node` has no TRUNCATE guard of its own.** Of 26 tables in `gwk`, 18 carry a
row-level delete guard and 17 carry the matching statement-level truncate cover.
`dispatch_node` is the one gap, and it is the exact pairing this schema's own comments warn
about — a row-level guard never fires on TRUNCATE. It is protected today, by accident: a
bare `TRUNCATE` is refused by the foreign key `gwk.cost_entry` holds on it, and
`TRUNCATE … CASCADE` is refused by `cost_entry`'s own guard. Neither refusal comes from
`dispatch_node`.

Not fixed here, deliberately. The fix edits `schema/0001_contract.sql`, which moves
`CONTRACT_SQL_SHA256`, which the digest gate would then correctly refuse until a new step
exists — doubling the live carry mid-phase for a table nothing can presently truncate. It
is the first customer of the machinery this phase built, and the fix having to arrive as a
step is the machinery working. The property is asserted in the meantime, independently of
which mechanism delivers it.

**The scratch rehearsal (C3) is not implemented.** It needs a base-shaped database and
nothing in the binary can produce one: `CONTRACT_SQL` is the *result* and a step is a
*delta*. Restoring an operator dump means shelling to `pg_restore`, which D3's own reasoning
forbids; `CREATE DATABASE … TEMPLATE live` needs zero sessions on the live database and the
writer lock is one. The compensating control is that the retroactive step is proven against
a base reconstructed from git — stronger than a rehearsal, because it compares the migrated
schema against a fresh initialization line for line — plus a CI check that every registered
step is named by that suite. `--dry-run` runs the rungs a read-only pass can answer and
applies nothing.

Four more were found by this phase's own goal-backward verification, after the
implementation was complete and every gate was green. They are narrowings rather
than defects — each guard does what it says, over a smaller subject than a reader
would assume — and they are recorded here because an unstated narrowing reads as
coverage.

**The DELETE arm proves itself against whatever rows the database happens to
hold.** The TRUNCATE arm covers all eighteen guarded relations without seeding
anything, because TRUNCATE needs no rows to be refused. The DELETE arm cannot:
`DELETE FROM t` over an empty table affects no rows, fires no row-level trigger,
and succeeds. So it skips every relation whose count is zero, and on a freshly
migrated database that leaves two — `gwk.transition`, seeded by the contract at
initialization, and the ledger's own row. The floor is `deleted_from == 0`, which
refuses a run that proved nothing whatsoever; it is not a claim that the other
sixteen row-level guards were exercised. They were not. A row-level guard that
went missing on a table that happens to be empty is invisible to this battery,
and what covers it instead is that the same table's statement-level guard is
checked unconditionally.

**The unit sweep covers one unit file, and the estate's units are not in this
repository.** Acceptance criterion 9 asks that the verb be absent from every
`.service` and `.timer`, and the test walks the repository asserting so. This
repository contains exactly one such file — `crates/gwk-pty-host/gwk-pty-host.service`
— and no timer at all. The units that actually run the estate live outside it.
The grep-proof is a statement about what this repository could ship, not about
what the operator's machine runs, and keeping the verb out of a unit on the host
stays an operator obligation the test does not discharge.

**A backend migration reaches an existing database only if a step names it, and
only a doc comment says so.** `BACKEND_MIGRATIONS` is applied wholesale by
`init`, so a seventh file added to that array reaches every fresh database
automatically. It reaches an existing one only through a step's `-- carries:`
header. Nothing enforces the pairing: adding `0007_*.sql` does not move
`CONTRACT_SQL_SHA256`, so the missing-step gate — which watches
`schema/0001_contract.sql` and nothing else — stays green while fresh and
migrated databases diverge. The positional guard between `BACKEND_MIGRATIONS` and
`BACKEND_MIGRATION_STEMS` asserts the two lists stay the same length; it says
nothing about whether a step carries the new entry.

**The two step gates disagree about an empty registry, and they fail closed.**
`inspect_step_chain` accepts a registry holding no steps when the contract still
carries the digest it carried when that gate was introduced — nothing has moved
that a step would have to describe. `resolve` refuses an empty registry
unconditionally, as `NoSteps`. A build can therefore pass CI and still refuse to
migrate anything. That is the safe direction and it is left alone: the CI gate
answers whether this repository owes a step, the runtime answers whether this
binary can carry a database, and the second question has no good answer from an
empty registry no matter what the contract digest is.

A fifth surfaced while closing one of those four, which is its own small argument for
writing the test the criterion actually names.

**`admin verify` over a migrated database guards the privilege arm, and only that.**
Criterion 1 asks that `verify` exit clean after a migration, and it now does, driven end to
end against the built binary. What that establishes is narrower than it reads. `verify`
refuses on two grounds — a runtime role holding a privilege, and a recorded contract digest
that is not this build's — and only the first is reachable from the migrate path. `migrate`
resolves the chain with `CONTRACT_SQL_SHA256` as its terminal target; `apply` re-reads
`schema_fingerprint` inside its own transaction and refuses on a mismatch; `assert_result`
re-reads it once more after commit. By the time `verify` looks, three guards have already
settled the comparison it is about to make, and an attempt to land a migration on a foreign
digest reds at `apply` without ever reaching the verb. The digest half of that clause is
guarded upstream, not here.

The privilege half is worth having on its own account. Role attributes do not appear in a
schema dump, so the scratch comparison the rest of this phase's proofs rest on cannot see a
migration that leaves the runtime role holding `CREATEDB` — and that is the mutation this
test reds on.

## What this cost to learn

Three claims in this phase were weaker than the prose around them. None was caught by
review; two fell to a mutation, and the third to goal-backward verification after every
gate was already green.

**The one-transaction guarantee was false when it was first claimed.** The applier wraps the
step, the backend migrations, the privilege matrix and the ledger row in one transaction —
and the step carried its own `BEGIN`/`COMMIT`, correct when a step was something applied by
hand. The step's `COMMIT` ended the applier's transaction, so everything after it ran
unprotected and a later failure would have rolled back nothing while reporting a refusal.
The seeded failing-step case is what found it. A step no longer opens a transaction, and the
generator refuses one that does.

**The count's presence carries the safety; its ordering carries the quality of the
message.** The guards in this phase assert a count before folding over what they counted,
and the stated reason was that `admin_init.rs` once went red on `26 != 22` before reaching
the per-table arm. Moving a count to run *after* its fold turned out to change nothing —
the count still fires. What produces a tautology is the count being *absent*, leaving the
fold as the only gate: a per-relation sweep over zero relations returns `Ok(())`. The
ordering is a diagnostics win, not a soundness one, and the distinction is worth stating
because the phase's own instructions had it the other way round until a mutation refused to
behave as specified.

**A count can be present, ordered first, and still slack.** The paragraph above was written
while `EXPECTED_TRUNCATE_GUARDS` was a floor — `guarded.len() < 17`, against a real count of
eighteen: seventeen in the contract and the eighteenth added by this phase, on the ledger's
own table. It is a count, it runs before the fold, and it was worth nothing. One guard of
slack is exactly enough to admit a database that lost one, and losing one removes the
relation from `pg_trigger`, which removes it from the set the per-relation probe walks — so
nothing downstream asked about it either. The battery returned `Ok(())` for a database a
superuser could TRUNCATE to zero rows.

The mutation that was supposed to catch this could not. `ALTER TABLE … DISABLE TRIGGER`
leaves the catalog row in place with `tgenabled = 'D'`, and the sweep's query filters on
neither column, so the relation stays in the set and the refusal comes from the probe. The
guard had only ever been watched failing in the one direction that never exercises the count
arm. A mutation has to remove the thing from the set the count is taken over, not merely
stop its effect; the constant is an equality now, and its mutation drops rather than
disables.
