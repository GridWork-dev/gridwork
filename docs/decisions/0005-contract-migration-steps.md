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

**B1 — always refuse a base the database is not at. `--from <sha256>` states a base and
gets it checked like any other.** This entry recorded B2 for most of the phase, and B2 was
never what the code did. B2 said the verb would apply a chain whose precondition it had not
checked, because a database whose fingerprint does not describe its actual schema is a real
state with no other exit. The implementation resolves the chain from `--from` and then
hands the same value to `assert_base`, which reads the recorded fingerprint and refuses on
a mismatch — so `--from` naming anything other than what the database records is refused,
and `--from` naming what it records changes nothing. There was never a third outcome.

Two reviewers found this independently, and it is recorded as B1 rather than repaired into
B2 on purpose: the code is stricter than the ruling, the stricter behaviour is the one worth
keeping, and a `data-migration` verb should not grow a documented bypass to make a document
true. **Cost, stated plainly:** the state B2 was ruled in to serve — a fingerprint that does
not describe its schema — **has no exit through this verb.** It needs a hand-authored step
or a restore.

The `asserted_base` column is gone from the ledger and the field from the receipt. It could
only ever have been written `true` for runs where the precondition *had* been checked, and
a permanent append-only record is the last place to keep a field that states the opposite
of what happened. What `--from` is good for now is stated in its own doc comment: an
operator who says out loud which base they believe they are on gets told when they are
wrong, before the writer lock costs anything.

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

That last sentence was wrong about one relation and is corrected below: `gwk.dispatch_node`
had no statement-level guard to fall back on, and the arm that should have covered it was
walking the wrong set. The set is now its own, and its count is an equality — which is what
now covers a row-level guard that goes missing on an empty table.

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

### What a REVIEW and SECURITY round found afterwards

Everything above was written while every gate was green. A code review, a security audit and
a migration-safety assessment then ran against the finished branch, and between them found
that the rehearsal could not run at all, that the protections battery had never exercised
three of its eighteen relations, and that a failed proof exited with the code meaning
*retry*. Those are fixed, and are recorded here because the pattern is the point: each one
was a guard whose prose was true and whose subject was smaller than the sentence implied.

**The TRUNCATE probe never reached three relations, in any run.** PostgreSQL checks a
table's inbound foreign keys *before* it fires any `BEFORE TRUNCATE` trigger — measured, not
reasoned. `gwk.attempt`, `gwk.task` and `gwk.context_manifest` are referenced by other
tables, so a bare `TRUNCATE` of them returned `0A000` from the foreign key while the guard
sat unexecuted, and the arm read that refusal as proof. Disabling one of those guards left
the battery green on both arms. Fixed twice over: the probe now truncates `CASCADE`, which
makes the guard actually run, and it requires the refusal to be `P0001` *naming that
relation* — because under CASCADE a neighbour's guard answers with a `P0001` of its own,
which is a true refusal about the wrong subject.

**The guard sweep could not see a disabled or downgraded trigger.** `pg_trigger` keeps the
row when a trigger is disabled, so a count over an unfiltered sweep never moved; and a guard
created without `ENABLE ALWAYS` sits at the default `ORIGIN`, which does not fire in a
replica session — the session a restore runs in. The sweep now filters `tgenabled = 'A'`, so
one predicate refuses a drop, a disable, and an `ALWAYS`-to-`ORIGIN` downgrade.

**`--dry-run` could not complete against a real database, and the rehearsal claims less than
it did.** The dry-run arm ran R3, whose relation count belongs to the *migrated* schema (35);
a dry run holds the database at its base, where the count is 27. Every rehearsal against a
real database therefore refused, with a message that read like schema corruption, after the
operator had already stopped the kernel to take the writer lock. R3 has moved inside the
applier's transaction where the count is correct, and the dry-run envelope now says
`"rungs_checked": ["base"]` and `"rehearsal": "not implemented"` rather than
`"grant_matrix": "checked"`. **A dry run is a preflight, not a proof** — it resolves the
chain and asserts the base.

**R3 and R4 ran after the commit.** Both are questions about the schema the step produced,
and the catalogue changes are visible inside the transaction, so running them afterwards
bought nothing and cost the ability to roll back: a step that widened the grant matrix or
broke an append-only guard committed first and was reported second. Both now run before
`tx.commit()`. R5 stays after it, because it exists to catch a writer that was never fenced
and a measurement taken inside the transaction cannot see outside it.

**Every rung failure exited 5, which this repository's own table defines as "retrying later
is the fix".** `KernelError::Schema` means something does not verify, whose code is 6 —
*retrying is NOT the fix*. A wrapper obeying the exit code would have re-run a migration
whose guards had just been found broken. The variant is now mapped rather than flattened.

**A post-commit failure printed no receipt.** The three rungs returned before the emit, so
the one case where an operator most needs the record — the schema has moved, the ledger row
exists, and something does not verify — was the one case that produced a single error line.
The receipt is now emitted either way, carrying `verified` and `verification_error`.

**`migrate` did not check the runtime role, and `public` was a legal role name.**
`admin::init` refuses a role holding `SUPERUSER`/`CREATEROLE`/`CREATEDB`/`BYPASSRLS`;
`migrate` replayed the same grant matrix and checked neither. `GWK_RUNTIME_ROLE` is read from
whatever environment the operator is in and nothing in the database records which role was
granted, so a stale export silently widens the matrix to a second role — invisibly, because
R3 and `verify` both re-read the same variable and find their own answer satisfied. Sharpest
corner: `public` matched the identifier pattern, and `GRANT … TO public` grants to every role
in the cluster. `migrate` now performs `init`'s check, and `validate_role` refuses `public`,
`current_user`, `session_user` and `current_role` as the `RoleSpec` keywords they are.

**No statement or lock timeout existed anywhere.** Every `ALTER TABLE` in the step takes
`ACCESS EXCLUSIVE`, and the writer lock is a `pg_try_advisory_lock` that excludes another
kernel writer and nothing else — not a psql session, not a dashboard read, not a `pg_dump`.
Any of them holding `ACCESS SHARE` made the first `ALTER` wait indefinitely, queueing every
later reader behind it. The applier's transaction now sets `lock_timeout = '5s'`.

### Still open, and disclosed rather than fixed

**The serve path never reads `schema_fingerprint`, so one deployment ordering is not loud.**
`daemon()` checks the revision stamp, the writer lock and the runtime privileges, and never
compares the recorded contract digest against the one it carries. Deploying new binaries
against an old schema fails fast and legibly, but for the wrong reason — a projection query
selects a column that does not exist. Migrating *before* advancing the pins does not fail
fast at all: the old binary's queries still resolve, it starts, claims an epoch, and serves,
and the first row it projects into the new shape violates a constraint at an arbitrary later
moment. **Advance the pins first, or keep the units stopped across both acts.** Do not rely
on the ordering being symmetric; it is not.

### And what a follow-up round closed

The two entries that stood above this line are gone from it, along with four gaps recorded
further up and one nobody had written down. Each was a guard reaching a smaller subject than
its prose, which is the same shape as everything else on this page and is why they are
listed rather than quietly fixed.

**The DELETE arm has its own relation set.** It walked the TRUNCATE arm's, on the assumption
that the two are the same relations. They are not — `gwk.dispatch_node` carries a row-level
delete guard and no statement-level cover, so the one table this record already singles out
was the one table neither arm ever named. The arm now sweeps for row-level DELETE triggers
directly (`tgtype & 1` and `tgtype & 8`, `tgenabled = 'A'`) and asserts an equality against
nineteen. The count matters more here than on the TRUNCATE side: that probe reaches every
relation it lists, and this one reaches only those holding a row when a migration runs, so
for all the others the count *is* the check.

**The ledger writes one row per step.** `migrations/0006` describes the table as holding one
row per applied step; `apply` performed a single INSERT with the ids comma-joined into
`step_id`. At six steps that string exceeds the column's `CHECK (length … BETWEEN 1 AND 128)`
and aborts the transaction on its last statement, after all the DDL has run. Six is now a
test, and five would not have been one: five fitted.

**`--dry-run` is exercised against a database.** The rehearsal arm had no behavioural test at
all, which is how the R3 count mismatch recorded above survived every gate — the receipt it
printed was on the path that never ran. A case now builds a base database, runs the built
binary, and asserts both halves: the plan comes back, and the fingerprint, the relation count
and the ledger table are where they were left. Only the second half catches a dry run that
prints a plan and applies anyway.

**The step-chain gate walks the chain.** It checked that no digest is based on twice, that
exactly one terminal exists, and that the terminal is the contract — all of which a line
*plus a closed cycle* satisfies, because no member of a cycle is anything's terminal. The
island's files read as applicable steps that no resolver will ever reach. The gate now walks
backward from the terminal and requires every registered step to be accounted for, which also
refuses a merge: two steps arriving at one digest make "the step before this one" a choice,
and the fork check cannot see it because it refuses two steps *leaving* a digest.

**The transaction-control guard is a scanner.** It tested whether a line *started* with one of
four keywords, and three shapes walked past it: a file ending `COMMIT` with no semicolon,
`SELECT 1; COMMIT;` on one line, and `END;` — PostgreSQL's synonym for COMMIT, left off the
list entirely because plpgsql closes every block with it. That last exclusion is why the fix
is a scanner and not a longer list: whether `END;` ends a transaction or a block is decided by
whether it sits inside a dollar-quoted body, which a line test cannot see.

**Backend migrations are pinned by digest.** `crates/gwk-kernel/migrations/` is applied at
initialization and never again, so editing a file there changes what every database created
afterwards carries and nothing about the ones created before. No digest moves, no step is
owed, and every gate stays green — the quietest way this schema can fork. All six files now
carry a byte pin the contract gate checks.

**The `KernelCommand` pin compares two artifacts.** Its guard against searching an empty
string was `variants.len() > 40` against a union of 56, which is sixteen variants of slack and
a number the file could be edited to agree with. It now compares the parsed variant names
against the discriminants in the generated bindings, with one fixed name anchoring the parse
so that two parses failing into empty sets cannot agree with each other.

## What this cost to learn

Five claims in this phase were weaker than the prose around them. None was caught by the
gates; two fell to a mutation, one to goal-backward verification after every gate was
already green, and two to a review round run after that verification had returned PASS.

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
leaves the catalog row in place with `tgenabled = 'D'`, and the sweep's query filtered on
neither column, so the relation stayed in the set and the refusal came from the probe. The
guard had only ever been watched failing in the one direction that never exercises the count
arm. A mutation has to remove the thing from the set the count is taken over, not merely
stop its effect; the constant is an equality now, and the sweep filters `tgenabled = 'A'`,
so a disable drops the relation out of the count as surely as a drop does.

**A probe can be structurally unable to reach the thing it probes, and reasoning will not
tell you.** The TRUNCATE arm looked sound for eighteen relations and was inert for three,
because PostgreSQL evaluates a table's inbound foreign keys before firing its `BEFORE
TRUNCATE` triggers. Nothing in the code, the schema or the comments said so, and the arm's
own failure mode — a refusal — was indistinguishable from success at doing its job. What
settled it was four statements against a throwaway container: guard present with an inbound
FK gave `0A000`, guard present without one gave `P0001`, and guard *disabled* with an
inbound FK gave a byte-identical `0A000`. Two of those three outcomes were the same, which
is the entire finding. The same experiment then refuted the obvious fix: `CASCADE` alone
lets a neighbour's guard answer with a `P0001` of its own, so the refusal has to be checked
for the relation it names. Both halves came from running it, not from reading it.

**A decision record can describe behaviour the code has never had, and stay that way through
every gate.** `--from` was ruled in as an escape hatch, documented as the one path where the
precondition goes unchecked, and given a receipt field and a permanent ledger column to
record that it had been used. It never did any of that: the value it supplies is handed
straight to the check it was supposed to bypass. Nothing was wrong with the code, and nothing
in this repository could have noticed — no test exercised the flag past the argument parser,
and the only test named for it called the applier directly, below the layer where the check
lives. Two reviewers reading independently both found it in the same pass. The lesson is
narrower than "write more tests": a flag whose entire purpose is to *relax* a guard has to be
tested through the layer that holds the guard, or the test cannot fail for the right reason.
