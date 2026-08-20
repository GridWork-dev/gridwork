//! Embeds the contract's migration steps into a Rust constant the contract
//! crate can carry.
//!
//! Same road as [`crate::schema`], for the same reason it took it: `schema/`
//! sits at the repo root, outside every package directory, and a `../../../`
//! reach out of a crate that will be published breaks at publish time rather
//! than at desk time, because `cargo package` copies only files under the
//! package root. The steps are that fact arriving a second time, so they get
//! the same answer — one authored file per step, one checked artifact, no
//! second source anyone can edit.
//!
//! # The step file format
//!
//! One step per file under `schema/steps/`, named `<base8>-<result8>.sql`,
//! where the two halves are the first eight characters of the two digests in
//! a two-line header:
//!
//! ```sql
//! -- base:   4b227777d4dd1fc61c6f884f48641d02b4d121d3fd328cb08b5531fcacdabf8a
//! -- result: ef2d127de37b942baad06145e54b0c619a1f22327b2ebbcfbec78f5564afe39d
//! ALTER TABLE gwk.task ADD COLUMN note text;
//! ```
//!
//! Both digests are 64 lowercase hex characters, and the file name has to
//! agree with them. That agreement is the whole point of naming the file after
//! its digests: a rename cannot silently retarget a step, and a step cannot
//! claim a base its name denies. Neither half is derivable from the other, so a
//! disagreement is refused here rather than resolved in favour of one of them.
//!
//! Every byte of the file becomes the step's `sql`, header lines included.
//! They are SQL comments, so carrying them costs nothing and keeps the embedded
//! bytes identical to the authored ones — a reader can diff a step against its
//! file without accounting for what the generator stripped.
//!
//! The generator does NOT decide which step is the last one in the chain, or
//! whether the steps form a chain at all. That is `gwk_kernel::migrate`'s job,
//! against the digest the binary actually carries; this module's contract stops
//! at "these files parsed, and their names match their headers".

use std::fmt::Write as _;
use std::path::Path;

use gwk_domain::is_sha256_hex;

/// Where the generated copy lands, relative to the repo root.
pub const GENERATED_PATH: &str = "crates/gwk-domain/src/contract_steps.rs";

/// Where the authored steps live, relative to the repo root.
pub const STEPS_DIR: &str = "schema/steps";

/// Where the kernel's backend migrations live, relative to the repo root.
pub const MIGRATIONS_DIR: &str = "crates/gwk-kernel/migrations";

/// The suite that applies steps to a real PostgreSQL, relative to the repo root.
pub const MIGRATE_SUITE: &str = "crates/gwk-kernel/tests/admin_migrate.rs";

/// The backend migrations a database at the chain's base already carries.
///
/// `schema/steps/` chains the CONTRACT digest. `gwk_internal` has no digest,
/// and `BACKEND_MIGRATIONS` is applied at initialization and never again — so a
/// migration added afterwards reaches every new database and no existing one.
/// The applier closes that by carrying them, and a step declares which ones it
/// carries; this constant is the other end of the accounting, the set that was
/// already in place before the first step could carry anything.
///
/// It is a claim about `4d54bba`, checkable with
/// `git show 4d54bba:crates/gwk-kernel/migrations/`, and it does not move: a
/// migration added today is claimed by a step, never appended here.
pub const INITIAL_BACKEND_MIGRATIONS: [&str; 4] = [
    "0001_kernel_internal",
    "0002_writer",
    "0003_blob",
    "0004_checkpoint",
];

/// Every backend migration, and the bytes it is frozen at.
///
/// The accounting above answers which migrations a database HAS. It cannot
/// answer whether they are still the ones it ran, and nothing else did either:
/// `crates/gwk-kernel/migrations/` is applied wholesale at initialization and
/// never again, so editing a file in it changes what every database created
/// afterwards carries and changes nothing about the ones created before. The
/// two then disagree, no digest moves, no step is owed, and every gate in this
/// repository stays green. It is the quietest way the schema can fork.
///
/// So the bytes are pinned. Every file gets a row, not only the four that
/// predate the chain — a migration a step carries is applied by that step to old
/// databases and by `init` to new ones, and editing it after either has happened
/// forks them the same way.
///
/// The cost is real and it is the point: an edit to one of these files fails the
/// contract gate until the digest here is updated by hand, which is the moment
/// to ask whether the edit is reachable by any database that already ran the
/// file. Usually it is not, and the answer is a new migration.
pub const FROZEN_BACKEND_MIGRATIONS: [(&str, &str); 6] = [
    (
        "0001_kernel_internal",
        "447b18dc57776efcba0206c9d295f0e9a8ffca1541c7310e9d6cc9ada0309036",
    ),
    (
        "0002_writer",
        "50b07b19c91bf58f8fbf07342e8b81d56bb81c07f66a57d458d3a93040122836",
    ),
    (
        "0003_blob",
        "c7d76d8aef2cb66624ec391b5d2e2da8427fe077748650c48776e5abf36250c0",
    ),
    (
        "0004_checkpoint",
        "cf22bc6f46465083c9b92547b5dd7a9a2ac86182e1d9cc59df62fdcf217d455f",
    ),
    (
        "0005_pty_delivery",
        "5d7e2205703c10a6a78b1d63931df9fe92856ee8b5c66814138fbe9ec3a6a2ad",
    ),
    (
        "0006_schema_migration",
        "358b405a42bfb8796408f429a53b511a47a6651a52f659bb20725ac657e15209",
    ),
];

/// How many characters of each digest a step's file name carries.
const NAME_PREFIX_LEN: usize = 8;

/// One step file, parsed and checked against its own name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedStep {
    /// The file name, which is the step's identity everywhere downstream.
    pub id: String,
    /// The contract digest a database must already carry to take this step.
    pub base: String,
    /// The contract digest a database carries once this step has been applied.
    pub result: String,
    /// The backend migrations this step carries, in the order they apply.
    ///
    /// Declared, never probed. Deciding it by looking for the relations a
    /// migration creates would need a migration-to-relation map, and a second
    /// hand-maintained list that can drift from the first is the thing this
    /// whole mechanism exists to remove.
    pub carries: Vec<String>,
    /// Every byte of the file.
    pub sql: String,
}

/// Read one `-- <field>:` header line and validate the digest on it.
fn header_digest(line: Option<&str>, field: &str, id: &str) -> Result<String, String> {
    let prefix = format!("-- {field}:");
    let line = line.ok_or_else(|| format!("{id}: has no `{prefix}` header line"))?;
    let digest = line
        .strip_prefix(&prefix)
        .ok_or_else(|| {
            format!("{id}: expected a `{prefix} <64 lowercase hex>` header line, found {line:?}")
        })?
        .trim();
    if !is_sha256_hex(digest) {
        return Err(format!(
            "{id}: `{prefix}` carries {digest:?}, which is not a 64-character lowercase hex digest"
        ));
    }
    Ok(digest.to_owned())
}

/// Read the `-- carries:` header line: the backend migrations this step brings.
///
/// The line is REQUIRED even when the list is empty. An absent line and a step
/// that carries nothing are different facts, and only one of them is a step
/// somebody finished writing — while an empty list is safe to state plainly,
/// because [`inspect_step_chain`] refuses a migration file no step claims.
fn header_carries(line: Option<&str>, id: &str) -> Result<Vec<String>, String> {
    let line = line.ok_or_else(|| {
        format!(
            "{id}: has no `-- carries:` header line. Every step declares the backend migrations \
             it brings forward, and a step that brings none says so with an empty list"
        )
    })?;
    let tail = line.strip_prefix("-- carries:").ok_or_else(|| {
        format!("{id}: expected a `-- carries: <names>` header line, found {line:?}")
    })?;

    let mut carries: Vec<String> = Vec::new();
    for name in tail
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        // The file stem, so the applier can find the file and the ledger row
        // reads as the thing on disk. Bounded to the shape those files have:
        // anything else is a typo that would otherwise become a missing file at
        // migration time, which is the worst moment to discover it.
        if name.len() < 6
            || !name.as_bytes()[..4].iter().all(u8::is_ascii_digit)
            || name.as_bytes()[4] != b'_'
            || !name
                .bytes()
                .skip(5)
                .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
        {
            return Err(format!(
                "{id}: `-- carries:` names {name:?}, which is not a `NNNN_lower_snake` migration \
                 stem"
            ));
        }
        if carries.iter().any(|seen| seen == name) {
            return Err(format!(
                "{id}: `-- carries:` names {name:?} twice — applying a migration twice is not \
                 something a list can mean"
            ));
        }
        carries.push(name.to_owned());
    }
    Ok(carries)
}

/// Parse one step file. `id` is its file name, `sql` every byte of it.
pub fn parse_step(id: &str, sql: &str) -> Result<ParsedStep, String> {
    // The two guards `schema.rs` carries, restated with a file name in the
    // message because there is more than one input here. A `"#` would close
    // the raw literal the emitter opens, and rustc normalizes CR away inside a
    // raw literal — so a CRLF file would compile to bytes that no longer match
    // the file the step's digests were computed over.
    if sql.contains("\"#") {
        return Err(format!(
            "{id}: contains `\"#`, which closes the raw string literal this generator emits — \
             widen the hash fence before adding it"
        ));
    }
    if sql.contains('\r') {
        return Err(format!(
            "{id}: has CR line endings; rustc would normalize them away inside the emitted \
             literal, and the embedded step would stop being the file it claims to be"
        ));
    }

    // Transaction control belongs to the applier, which wraps the step, the
    // backend migrations, the privilege matrix and the ledger row in one. A
    // COMMIT in a step ends THAT transaction: everything after it runs outside
    // one, and a later failure leaves the step committed while reporting that
    // nothing was applied. Refused here because the symptom appears three
    // components away from the cause.
    if let Some((line, keyword)) = transaction_control(sql) {
        return Err(format!(
            "{id}:{line}: carries `{keyword}`, and a step does not own its transaction — the \
             applier wraps it together with the backend migrations, the privilege matrix and the \
             ledger row, and a COMMIT here ends that"
        ));
    }

    let mut lines = sql.lines();
    let base = header_digest(lines.next(), "base", id)?;
    let result = header_digest(lines.next(), "result", id)?;
    let carries = header_carries(lines.next(), id)?;
    if base == result {
        return Err(format!(
            "{id}: base and result are both {base} — a step that arrives where it started is not \
             a step"
        ));
    }

    let stem = id
        .strip_suffix(".sql")
        .ok_or_else(|| format!("{id}: a step file name ends in `.sql`"))?;
    let (base8, result8) = stem.split_once('-').ok_or_else(|| {
        format!(
            "{id}: a step file is named `<base8>-<result8>.sql` — the first {NAME_PREFIX_LEN} \
             characters of each header digest, separated by a dash"
        )
    })?;
    for (half, field, digest) in [(base8, "base", &base), (result8, "result", &result)] {
        if half.len() != NAME_PREFIX_LEN || !digest.starts_with(half) {
            return Err(format!(
                "{id}: the file name's {field} half is {half:?} and the `-- {field}:` header is \
                 {digest:?} — the two have to agree, so that a rename cannot retarget a step and \
                 a step cannot claim a base its name denies"
            ));
        }
    }

    Ok(ParsedStep {
        id: id.to_owned(),
        base,
        result,
        carries,
        sql: sql.to_owned(),
    })
}

/// Read and parse every `*.sql` under `dir`, in file-name order.
///
/// A missing directory is a hard failure rather than zero steps. The two are
/// indistinguishable once they reach the emitter, and "the path moved" would
/// read exactly like "nobody has authored a migration yet" — the wrong
/// diagnosis of the right symptom. An EMPTY directory is a legitimate state
/// with a legitimate answer: an empty registry, which the resolver refuses in
/// its own words.
pub fn read_steps(dir: &Path) -> Vec<ParsedStep> {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|err| {
        panic!(
            "read {}: {err} (the directory is authored, not generated — \
             an absent one means the path moved, not that there are no steps)",
            dir.display()
        )
    });

    let mut names: Vec<String> = Vec::new();
    for entry in entries {
        let path = entry
            .unwrap_or_else(|err| panic!("read an entry of {}: {err}", dir.display()))
            .path();
        if path.extension().is_none_or(|ext| ext != "sql") {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            panic!("{}: step file names are UTF-8", path.display());
        };
        names.push(name.to_owned());
    }

    // Emission order is decided here and nowhere else. `read_dir` hands back
    // whatever order the filesystem stored, so an unsorted read would emit a
    // different artifact on a different machine and the drift gate would fire
    // on a change nobody made.
    names.sort();

    names
        .iter()
        .map(|name| {
            let sql = std::fs::read_to_string(dir.join(name))
                .unwrap_or_else(|err| panic!("read {}: {err}", dir.join(name).display()));
            parse_step(name, &sql).unwrap_or_else(|message| panic!("{message}"))
        })
        .collect()
}

/// The SHA-256 of `schema/0001_contract.sql` as it stood when this gate was
/// introduced.
///
/// It is NOT the first contract this repository shipped, and nothing here
/// should be read as a claim that it is: `git log schema/0001_contract.sql`
/// lists more than twenty digests before this one, and a database is running on
/// one of them. This constant makes no statement about history. It is one fixed
/// point, and it does one job.
///
/// That job is to say when an EMPTY `schema/steps/` is tolerable — which is
/// only while the contract still carries this exact digest. The moment the SQL
/// changes, a database on the older contract needs a step, and noticing that
/// requires something to compare against that this command does not itself
/// rewrite. `CONTRACT_SQL_SHA256` cannot be that something: it is regenerated
/// from the same bytes a moment earlier, so comparing the two agrees by
/// construction and would pass every contract change ever made.
///
/// It is a bootstrap and it expires. Once ANY step is registered the
/// empty-registry branch is unreachable and this value is never read again.
/// When it stops matching the file, the honest move is to delete it and that
/// branch — not to refresh it.
pub const CONTRACT_SHA256_AT_GATE_INTRODUCTION: &str =
    "7ebb2adaad295c28c60f1e789030ceef87d6fdd607b37a1186f173dc22647142";

/// Refuse a contract digest that no authored step arrives at.
///
/// `contract_sql` is the bytes of `schema/0001_contract.sql`; the digest
/// compared against is computed HERE, from those bytes, and never read back out
/// of `crates/gwk-domain/src/contract_sql.rs`. See
/// [`CONTRACT_SHA256_AT_GATE_INTRODUCTION`] for why that distinction is the
/// whole gate.
///
/// Two arms are already settled before anything reaches here, and are not
/// restated: [`parse_step`] refuses a file whose name disagrees with its header
/// and a step whose base equals its result, and [`read_steps`] surfaces both as
/// a hard failure. What is left is what no single file can know — whether the
/// SET of them is a line, and whether that line ends where this contract is.
pub fn inspect_step_chain(contract_sql: &str, steps: &[ParsedStep]) -> Result<(), String> {
    let current = crate::schema::sha256_hex(contract_sql.as_bytes());

    // The count decides first. An empty registry is a state, not a chain, and
    // walking it would find nothing and report that nothing was wrong — so
    // whether it is legitimate is settled before anything else looks at it.
    //
    // It is legitimate in exactly one case: the contract still carries the
    // digest it carried when this gate was introduced, so nothing has moved
    // that a step would have to describe. A registry that HAS steps is not
    // second-guessed here on either side of that line; it goes to the chain
    // checks below, where the only question worth asking is whether it arrives
    // at the contract this repository actually ships.
    let registered = steps.len();

    if registered == 0 {
        if current == CONTRACT_SHA256_AT_GATE_INTRODUCTION {
            return Ok(());
        }
        return Err(format!(
            "no migration step results in {current}, the contract digest \
             schema/0001_contract.sql now carries: schema/steps/ holds no step files at all. \
             The contract has moved off {CONTRACT_SHA256_AT_GATE_INTRODUCTION}, the digest it \
             carried when this gate was introduced, and a database on an older contract has no \
             way to follow it — author schema/steps/<base8>-{}.sql",
            &current[..NAME_PREFIX_LEN]
        ));
    }

    // A base claimed twice is a fork, and a fork makes the terminal below a
    // choice rather than a fact. Refused over the whole set, not over the route
    // anyone happens to want.
    let mut claimed: Vec<(&str, &str)> = Vec::with_capacity(registered);
    for step in steps {
        if let Some((_, first)) = claimed.iter().find(|(base, _)| *base == step.base) {
            return Err(format!(
                "steps {first} and {} both base on {}: the migration chain is a line, and two \
                 ways forward from one digest would make choosing between them a solver",
                step.id, step.base
            ));
        }
        claimed.push((step.base.as_str(), step.id.as_str()));
    }

    // The terminal is the one step whose result nothing else bases on. Counted
    // before it is read: a chain that loops has no terminal and a chain that
    // forks at the end has two, and `steps.iter().find(..)` would report the
    // first of them as though it were the only one.
    let terminals: Vec<&ParsedStep> = steps
        .iter()
        .filter(|step| !steps.iter().any(|other| other.base == step.result))
        .collect();
    if terminals.len() != 1 {
        return Err(format!(
            "expected exactly one terminal step — one whose result no other step bases on — and \
             found {} among the {registered} registered: {}",
            terminals.len(),
            step_ids(steps)
        ));
    }
    let terminal = terminals[0];

    if terminal.result != current {
        return Err(format!(
            "no migration step results in {current}, the contract digest \
             schema/0001_contract.sql now carries: the chain ends at {} ({}). Either the SQL \
             changed without a step, or the last step records the wrong result",
            terminal.result, terminal.id
        ));
    }

    // Everything above is about the ends of the chain, and a set can satisfy all
    // of it while not being one chain. Two disjoint lines are refused, because
    // they have two terminals — but a line PLUS a closed cycle has exactly one,
    // since no member of a cycle is anything's terminal, and every check so far
    // passes over a registry holding a chain and an island that nothing reaches.
    // The island's steps are files an operator will read as applicable and a
    // resolver will never walk.
    //
    // So walk it. Backward from the terminal, one hop per step, and require the
    // walk to account for every file registered. A step reached by the walk is a
    // step some database can actually take; a step the walk never reaches is
    // not, whatever its header says.
    //
    // Bounded by `registered` rather than trusted to terminate. Unique bases and
    // a single terminal do rule out a cycle reachable from it — but a build gate
    // that hangs is a worse failure than one that reports the wrong reason, and
    // the bound costs a comparison.
    let mut walked = vec![terminal];
    while walked.len() <= registered {
        let cursor = walked[walked.len() - 1];
        let predecessors: Vec<&ParsedStep> = steps
            .iter()
            .filter(|other| other.result == cursor.base)
            .collect();
        match predecessors.as_slice() {
            // The chain's start: nothing results in the digest this step bases
            // on, which is what being first means.
            [] => break,
            [one] => walked.push(one),
            // Two steps arriving at one digest. The fork check above cannot see
            // this one — it refuses two steps LEAVING a digest — and a merge
            // breaks the walk the same way, by making "the step before this one"
            // a choice.
            many => {
                return Err(format!(
                    "{} steps result in {}, which {} bases on: {} — a merge makes the step before \
                     it a choice, and the chain is a line",
                    many.len(),
                    cursor.base,
                    cursor.id,
                    step_ids_of(many)
                ));
            }
        }
    }
    if walked.len() != registered {
        let reached: Vec<&str> = walked.iter().map(|step| step.id.as_str()).collect();
        let stranded: Vec<&ParsedStep> = steps
            .iter()
            .filter(|step| !reached.contains(&step.id.as_str()))
            .collect();
        return Err(format!(
            "the chain from {} back reaches {} of the {registered} registered steps: {} is \
             registered and unreachable — a step no walk from the contract arrives at is a file \
             that reads as applicable and never applies",
            terminal.id,
            walked.len(),
            step_ids_of(&stranded)
        ));
    }

    Ok(())
}

/// Refuse a backend migration no step claims, or one two steps both claim.
///
/// `crates/gwk-kernel/migrations/` is applied wholesale at initialization and
/// never again, so every file in it either was already in place at the chain's
/// base ([`INITIAL_BACKEND_MIGRATIONS`]) or has to be carried forward by exactly
/// one step. A file nobody claims reaches new databases and no existing one and
/// nothing says so; a file two steps claim gets applied twice, and the second
/// application is an error at the worst possible moment.
///
/// `present` is the migration stems found on disk. Counted before anything
/// folds, because a directory read that returned nothing would make every
/// "claimed exactly once" question vacuously true.
pub fn inspect_backend_migration_claims(
    present: &[String],
    steps: &[ParsedStep],
) -> Result<(), String> {
    if present.is_empty() {
        return Err(format!(
            "no backend migrations found under {MIGRATIONS_DIR}: this check is about which of \
             them a step claims, and over an empty set every answer is yes"
        ));
    }

    // Claimed twice, checked across the whole registry rather than per step —
    // `parse_step` already refuses one step naming a migration twice, and this
    // is the other shape of the same mistake.
    let mut claimed: Vec<(&str, &str)> = Vec::new();
    for step in steps {
        for name in &step.carries {
            if let Some((_, first)) = claimed.iter().find(|(seen, _)| seen == name) {
                return Err(format!(
                    "backend migration {name:?} is carried by both {first} and {}: applying it \
                     twice is not something two steps can agree to do",
                    step.id
                ));
            }
            claimed.push((name.as_str(), step.id.as_str()));
        }
    }

    // Every claim names a file that exists. A step carrying a migration that
    // was renamed or removed fails at migration time otherwise, against a live
    // database, inside the transaction.
    for (name, id) in &claimed {
        if !present.iter().any(|stem| stem == name) {
            return Err(format!(
                "{id} carries backend migration {name:?}, and no such file exists under \
                 {MIGRATIONS_DIR}"
            ));
        }
    }

    // And every file is accounted for, by a step or by the initial set.
    let unclaimed: Vec<&String> = present
        .iter()
        .filter(|stem| {
            !INITIAL_BACKEND_MIGRATIONS.contains(&stem.as_str())
                && !claimed.iter().any(|(name, _)| name == *stem)
        })
        .collect();
    if !unclaimed.is_empty() {
        return Err(format!(
            "backend migration(s) {} are carried by no step and are not in the set a database at \
             the chain's base already has. `BACKEND_MIGRATIONS` runs at initialization and never \
             again, so an unclaimed file reaches every NEW database and no existing one — name it \
             on a step's `-- carries:` line",
            unclaimed
                .iter()
                .map(|stem| format!("{stem:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    Ok(())
}

/// Refuse a backend migration whose bytes have moved since they were pinned.
///
/// See [`FROZEN_BACKEND_MIGRATIONS`] for why the bytes are the thing being
/// checked. `present` is the stems found on disk, and it is compared as a SET
/// against the pin table in both directions: a file with no pin is as much a
/// hole as a pin whose file has changed, because the first thing an unpinned
/// file can do is change.
pub fn inspect_frozen_backend_migrations(dir: &Path, present: &[String]) -> Result<(), String> {
    // Counted first. Over an empty directory every "matches its pin" question
    // below is vacuously true, and a read that returned nothing would report a
    // clean set of frozen migrations for a repository that has none.
    if present.len() != FROZEN_BACKEND_MIGRATIONS.len() {
        let pinned: Vec<&str> = FROZEN_BACKEND_MIGRATIONS
            .iter()
            .map(|(id, _)| *id)
            .collect();
        return Err(format!(
            "{} backend migrations are pinned and {} are on disk under {MIGRATIONS_DIR}: pinned \
             {pinned:?}, present {present:?}. A file with no pin is one whose bytes nothing \
             watches",
            FROZEN_BACKEND_MIGRATIONS.len(),
            present.len()
        ));
    }

    for (stem, expected) in FROZEN_BACKEND_MIGRATIONS {
        let path = dir.join(format!("{stem}.sql"));
        let sql = std::fs::read_to_string(&path)
            .map_err(|err| format!("read {}: {err}", path.display()))?;
        let actual = crate::schema::sha256_hex(sql.as_bytes());
        if actual != expected {
            return Err(format!(
                "{MIGRATIONS_DIR}/{stem}.sql now digests to {actual} and is pinned at {expected}. \
                 This file is applied at initialization and never again, so the edit reaches every \
                 database created after it and none created before — if that difference is \
                 intended, it belongs in a NEW migration a step carries, and if the file was \
                 never applied anywhere the pin is what moves"
            ));
        }
    }

    Ok(())
}

/// The migration stems on disk, in file-name order.
pub fn read_backend_migrations(dir: &Path) -> Vec<String> {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|err| panic!("read {}: {err}", dir.display()));
    let mut stems: Vec<String> = Vec::new();
    for entry in entries {
        let path = entry
            .unwrap_or_else(|err| panic!("read an entry of {}: {err}", dir.display()))
            .path();
        if path.extension().is_none_or(|ext| ext != "sql") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            panic!("{}: migration file names are UTF-8", path.display());
        };
        stems.push(stem.to_owned());
    }
    stems.sort();
    stems
}

/// Refuse a registered step no integration test names.
///
/// Deliberately crude: a substring search for each step's file name in the
/// migrate suite's source. It cannot tell a real proof from the word appearing
/// in a comment, and it is not meant to. Task 3's reconstruction is stronger
/// than any rehearsal for the step it covers, and it exists because somebody
/// wrote it — this is the reminder that somebody has to.
///
/// One step does not justify generalising that reconstruction, so this does not
/// try. It is a tripwire across the gap between "a step is registered" and "a
/// step has ever been applied to a database".
pub fn inspect_step_coverage(steps: &[ParsedStep], suite: &str) -> Result<(), String> {
    // Counted before anything is searched for. Over zero registered steps
    // "every step is covered" is true of nothing, and the guard would report a
    // proven registry it never looked at.
    if steps.is_empty() {
        return Err(format!(
            "no steps are registered, so every step being covered is true of nothing: this \
             check is about {STEPS_DIR} holding entries the suite exercises"
        ));
    }
    let uncovered: Vec<&str> = steps
        .iter()
        .map(|step| step.id.as_str())
        .filter(|id| !suite.contains(*id))
        .collect();
    if !uncovered.is_empty() {
        return Err(format!(
            "step(s) {} are registered and named nowhere in {MIGRATE_SUITE}. A step no test has \
             ever applied to a database is DDL that has been reviewed and never run",
            uncovered
                .iter()
                .map(|id| format!("{id:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(())
}

/// Transaction control in `sql` that the applier's transaction would execute,
/// as the line it sits on and the keyword it is.
///
/// The line-oriented predecessor to this asked whether a line STARTED with one
/// of four keywords, and three shapes walked past it. A file whose last line is
/// `COMMIT` — no semicolon, nothing after it — matched neither `COMMIT;` nor a
/// `COMMIT ` prefix. `SELECT 1; COMMIT;` put the keyword where no line-start
/// test looks. And `END;`, which PostgreSQL treats as a synonym for COMMIT, was
/// left off the list entirely, because plpgsql closes every block with it and a
/// step made of DO blocks — which every step here is — would be unwritable.
///
/// That third one is why this is a scanner and not a longer keyword list.
/// Whether `END;` closes a transaction or a plpgsql block is decided by whether
/// it sits inside a dollar-quoted body: a line test cannot see that and this
/// can, so the synonym is refused at the top level and still allowed in the one
/// place every DO block needs it.
fn transaction_control(sql: &str) -> Option<(usize, String)> {
    // Comments, string literals and dollar-quoted bodies become blanks, one byte
    // for one byte, so every offset and line break in the result is the one it
    // had in the input. Nothing here parses SQL — it establishes what is at the
    // TOP level, which is the whole of what the question turns on.
    let scrubbed = scrub_to_top_level(sql);

    let mut offset = 0usize;
    for statement in scrubbed.split(';') {
        let start = offset;
        offset += statement.len() + 1;
        let leading = statement.len() - statement.trim_start().len();
        let Some(word) = statement.split_whitespace().next() else {
            continue;
        };
        let keyword = word.to_ascii_uppercase();
        // `START` opens one and `ABORT` ends one, as surely as the three that
        // were already listed; `END` can only be the statement by the time a
        // word reaches here, because a plpgsql block's is inside a body that was
        // blanked before the split.
        if !["BEGIN", "START", "COMMIT", "END", "ROLLBACK", "ABORT"].contains(&keyword.as_str()) {
            continue;
        }
        let line = 1 + scrubbed[..start + leading]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count();
        return Some((line, keyword));
    }
    None
}

/// `sql` with every comment, string literal and dollar-quoted body blanked out.
///
/// Byte for byte, and newlines survive as newlines: the caller reports a line
/// number out of the result and it has to be the line number in the file.
fn scrub_to_top_level(sql: &str) -> String {
    fn blank(out: &mut Vec<u8>, byte: u8) {
        out.push(if byte == b'\n' { b'\n' } else { b' ' });
    }

    let bytes = sql.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        // `--` to the end of the line.
        if bytes[i] == b'-' && bytes.get(i + 1) == Some(&b'-') {
            while i < bytes.len() && bytes[i] != b'\n' {
                blank(&mut out, bytes[i]);
                i += 1;
            }
            continue;
        }
        // `/* .. */`, which nests in PostgreSQL and is counted rather than
        // closed at the first `*/`.
        if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
            let mut depth = 1usize;
            blank(&mut out, bytes[i]);
            blank(&mut out, bytes[i + 1]);
            i += 2;
            while i < bytes.len() && depth > 0 {
                if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                } else if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                    depth += 1;
                } else {
                    blank(&mut out, bytes[i]);
                    i += 1;
                    continue;
                }
                blank(&mut out, bytes[i]);
                blank(&mut out, bytes[i + 1]);
                i += 2;
            }
            continue;
        }
        // A single-quoted literal, in which `''` is an escaped quote rather than
        // the end — reading it as the end would leave the rest of the literal
        // scanned as though it were statements.
        if bytes[i] == b'\'' {
            blank(&mut out, bytes[i]);
            i += 1;
            while i < bytes.len() {
                let quote = bytes[i] == b'\'';
                let escaped = quote && bytes.get(i + 1) == Some(&b'\'');
                blank(&mut out, bytes[i]);
                i += 1;
                if escaped {
                    blank(&mut out, bytes[i]);
                    i += 1;
                } else if quote {
                    break;
                }
            }
            continue;
        }
        // `$tag$ .. $tag$`, the form every DO block in a step is written in.
        if bytes[i] == b'$'
            && let Some(tag_len) = dollar_tag(&bytes[i..])
        {
            let tag = bytes[i..i + tag_len].to_vec();
            for _ in 0..tag_len {
                blank(&mut out, bytes[i]);
                i += 1;
            }
            while i < bytes.len() {
                if bytes[i..].starts_with(&tag) {
                    for _ in 0..tag_len {
                        blank(&mut out, bytes[i]);
                        i += 1;
                    }
                    break;
                }
                blank(&mut out, bytes[i]);
                i += 1;
            }
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    // Never lossy, and never defaulted to empty: blanks are ASCII and every
    // other byte is copied in place, so no multi-byte character is split. An
    // empty string here would make the caller's scan vacuous, which is the one
    // failure this must not degrade into quietly.
    String::from_utf8(out).expect("blanking replaces bytes one for one")
}

/// The length of the dollar-quote tag that opens `bytes`, if one does.
///
/// `$$` is a tag of two bytes and `$body$` one of six. A `$` followed by
/// anything that cannot be a tag — `$1`, a parameter placeholder — is not one,
/// and answering `None` there is what stops a placeholder from swallowing the
/// rest of the file as a quoted body.
fn dollar_tag(bytes: &[u8]) -> Option<usize> {
    let mut end = 1usize;
    while end < bytes.len() {
        match bytes[end] {
            b'$' => return Some(end + 1),
            b'A'..=b'Z' | b'a'..=b'z' | b'_' => end += 1,
            // A digit cannot open a tag, which is what distinguishes `$1` from
            // `$q1$`.
            b'0'..=b'9' if end > 1 => end += 1,
            _ => return None,
        }
    }
    None
}

/// The step ids, for a message that would otherwise name a count and nothing to
/// look at.
fn step_ids(steps: &[ParsedStep]) -> String {
    steps
        .iter()
        .map(|step| step.id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The same, for the borrowed subsets the chain walk assembles.
fn step_ids_of(steps: &[&ParsedStep]) -> String {
    steps
        .iter()
        .map(|step| step.id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The generated module for `steps`, byte for byte what belongs on disk.
///
/// The slice carries `#[rustfmt::skip]`, which is not decoration. `cargo fmt
/// --all` formats generated files too, and rustfmt's opinion about this literal
/// DEPENDS ON HOW MANY ELEMENTS IT HAS: it collapses a one-element slice onto
/// `&[Step { .. }]` and leaves a two-element one broken across lines. No single
/// emitted layout can satisfy both, so a generator that tried would produce a
/// file `cargo fmt --check` reds on the day a second step is authored — visible
/// as permanent contract drift with nothing to fix. Skipping the item hands the
/// layout to the one thing that knows the count.
pub fn contract_steps_rs(steps: &[ParsedStep]) -> String {
    let mut out = String::from(
        "// The contract's migration steps, embedded from schema/steps/*.sql\n\
         // by `cargo run -p xtask -- contract`.\n\
         // DO NOT EDIT — regenerate instead; CI diffs this file against the source.\n\
         \n\
         /// One authored migration step: the DDL that carries a database from the\n\
         /// contract digest in `base` to the one in `result`.\n\
         #[derive(Debug, Clone, Copy, PartialEq, Eq)]\n\
         pub struct Step {\n    \
             /// The step's file name under `schema/steps/`. It is the identity a\n    \
             /// receipt and a ledger row will name, which is why it is carried rather\n    \
             /// than an index into [`CONTRACT_STEPS`] — an index moves the moment a\n    \
             /// step is inserted.\n    \
             pub id: &'static str,\n    \
             /// The contract digest a database must already carry to take this step.\n    \
             pub base: &'static str,\n    \
             /// The contract digest a database carries once this step has been applied.\n    \
             pub result: &'static str,\n    \
             /// The backend migrations this step carries, in the order they apply.\n    \
             ///\n    \
             /// `gwk_internal` has no digest and no chain of its own, so a migration\n    \
             /// added after a database was initialized reaches it only if something\n    \
             /// carries it. This is that declaration; the applier reads it, and the\n    \
             /// ledger row records it, so a row never credits the step with work the\n    \
             /// step did not do.\n    \
             pub backend_migrations: &'static [&'static str],\n    \
             /// Every byte of the step's file, header comments included.\n    \
             pub sql: &'static str,\n\
         }\n\
         \n\
         /// Every step under `schema/steps/`, in file-name order and no other order.\n\
         /// The order they are APPLIED in is a property of the digests, not of this\n\
         /// slice, and `gwk_kernel::migrate` is what reads it out of them.\n\
         // Laid out by the generator: rustfmt formats one element differently from\n\
         // two, so no emitted shape survives both. See xtask/src/steps.rs.\n\
         #[rustfmt::skip]\n",
    );

    if steps.is_empty() {
        out.push_str("pub const CONTRACT_STEPS: &[Step] = &[];\n");
        return out;
    }

    out.push_str("pub const CONTRACT_STEPS: &[Step] = &[\n");
    for step in steps {
        let ParsedStep {
            id,
            base,
            result,
            sql,
            ..
        } = step;
        let carries = if step.carries.is_empty() {
            "&[]".to_owned()
        } else {
            format!(
                "&[{}]",
                step.carries
                    .iter()
                    .map(|name| format!("{name:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let _ = write!(
            out,
            "    Step {{\n        \
                 id: {id:?},\n        \
                 base: {base:?},\n        \
                 result: {result:?},\n        \
                 backend_migrations: {carries},\n        \
                 sql: r#\"{sql}\"#,\n    \
             }},\n"
        );
    }
    out.push_str("];\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";
    const D: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";
    const E: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

    /// A step file carrying no backend migrations — the shape most of these
    /// cases are about, since the digests and the file name are what they test.
    fn step_file(base: &str, result: &str, body: &str) -> String {
        step_file_carrying(base, result, &format!("-- carries:\n{body}"))
    }

    #[test]
    fn the_test_digests_are_the_shape_the_parser_demands() {
        // Every case below leans on these being valid; a typo in one would
        // otherwise show up as the parser rejecting a file for the wrong reason.
        assert!(is_sha256_hex(A));
        assert!(is_sha256_hex(B));
        assert!(is_sha256_hex(C));
    }

    #[test]
    fn a_well_formed_step_carries_every_byte_of_its_file() {
        let sql = step_file(A, B, "ALTER TABLE gwk.task ADD COLUMN note text;\n");
        let parsed = parse_step("aaaaaaaa-bbbbbbbb.sql", &sql).expect("well-formed step");
        assert_eq!(parsed.id, "aaaaaaaa-bbbbbbbb.sql");
        assert_eq!(parsed.base, A);
        assert_eq!(parsed.result, B);
        assert_eq!(parsed.sql, sql, "the header travels with the DDL");
    }

    #[test]
    fn a_file_name_that_disagrees_with_the_header_is_refused() {
        let sql = step_file(A, B, "SELECT 1;\n");
        let err = parse_step("aaaaaaaa-cccccccc.sql", &sql).expect_err("name disagrees");
        assert!(err.contains("result half is \"cccccccc\""), "{err}");
        // The message has to name BOTH sides: which one is wrong is exactly
        // what the generator cannot know.
        assert!(err.contains(B), "{err}");
    }

    #[test]
    fn a_truncated_file_name_half_is_refused() {
        let sql = step_file(A, B, "SELECT 1;\n");
        let err = parse_step("aaaaaaa-bbbbbbbb.sql", &sql).expect_err("seven characters");
        assert!(err.contains("base half is \"aaaaaaa\""), "{err}");
    }

    #[test]
    fn a_header_that_is_not_lowercase_hex_is_refused() {
        let sql = step_file(&A.to_uppercase(), B, "SELECT 1;\n");
        let err = parse_step("AAAAAAAA-bbbbbbbb.sql", &sql).expect_err("uppercase digest");
        assert!(
            err.contains("not a 64-character lowercase hex digest"),
            "{err}"
        );
    }

    #[test]
    fn a_missing_header_line_is_refused() {
        let err = parse_step("aaaaaaaa-bbbbbbbb.sql", "SELECT 1;\n").expect_err("no header");
        assert!(err.contains("expected a `-- base:"), "{err}");
        let only_base = format!("-- base:   {A}\n");
        let err = parse_step("aaaaaaaa-bbbbbbbb.sql", &only_base).expect_err("half a header");
        assert!(err.contains("has no `-- result:` header line"), "{err}");
    }

    #[test]
    fn a_step_that_arrives_where_it_started_is_refused() {
        let sql = step_file(A, A, "SELECT 1;\n");
        let err = parse_step("aaaaaaaa-aaaaaaaa.sql", &sql).expect_err("base equals result");
        assert!(err.contains("arrives where it started"), "{err}");
    }

    #[test]
    fn a_hash_fence_collision_is_refused() {
        let sql = step_file(A, B, "SELECT '\"#';\n");
        let err = parse_step("aaaaaaaa-bbbbbbbb.sql", &sql).expect_err("hash fence");
        assert!(err.contains("closes the raw string literal"), "{err}");
    }

    #[test]
    fn carriage_returns_are_refused() {
        let sql = format!("-- base:   {A}\r\n-- result: {B}\r\nSELECT 1;\r\n");
        let err = parse_step("aaaaaaaa-bbbbbbbb.sql", &sql).expect_err("CRLF");
        assert!(err.contains("CR line endings"), "{err}");
    }

    #[test]
    fn an_empty_directory_yields_an_empty_registry_and_a_missing_one_does_not() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(read_steps(dir.path()).len(), 0);

        let gone = dir.path().join("moved");
        let missing = std::panic::catch_unwind(|| read_steps(&gone));
        assert!(missing.is_err(), "a missing directory is not zero steps");
    }

    #[test]
    fn steps_are_read_in_file_name_order_and_nothing_else_is_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("bbbbbbbb-aaaaaaaa.sql"),
            step_file(B, A, "SELECT 2;\n"),
        )
        .expect("write");
        std::fs::write(
            dir.path().join("aaaaaaaa-bbbbbbbb.sql"),
            step_file(A, B, "SELECT 1;\n"),
        )
        .expect("write");
        std::fs::write(dir.path().join("README.md"), "not a step\n").expect("write");

        let steps = read_steps(dir.path());
        // Count before reading anything out of it: a per-element assertion on
        // an empty vector passes without inspecting a thing.
        assert_eq!(steps.len(), 2, "two `.sql` files, and the `.md` is not one");
        assert_eq!(steps[0].id, "aaaaaaaa-bbbbbbbb.sql");
        assert_eq!(steps[1].id, "bbbbbbbb-aaaaaaaa.sql");
    }

    // The gate's arms, in the order it applies them. CI's seeded control
    // exercises exactly one of them — the zero-step arm, because that is the
    // state a real contract change lands in — so the rest are checked here or
    // nowhere.

    /// A step file whose header and name agree, over SQL the gate never reads.
    fn parsed(base: &str, result: &str) -> ParsedStep {
        let id = format!(
            "{}-{}.sql",
            &base[..NAME_PREFIX_LEN],
            &result[..NAME_PREFIX_LEN]
        );
        parse_step(&id, &step_file(base, result, "SELECT 1;\n")).expect("well-formed step")
    }

    /// The real `schema/0001_contract.sql`, whose digest is
    /// [`CONTRACT_SHA256_AT_GATE_INTRODUCTION`] until somebody changes the
    /// contract. Read from disk rather than synthesized: the anchor is only
    /// meaningful against the bytes it was taken from.
    fn anchored_sql() -> String {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask has a parent dir")
            .join("schema/0001_contract.sql");
        std::fs::read_to_string(path).expect("read schema/0001_contract.sql")
    }

    // The gate has four states, and each has a test below. The two that matter
    // most are the ones that look alike: an empty registry is tolerated at the
    // anchor and refused off it, and that difference is the entire reason the
    // anchor exists.

    /// A step declaring `carries`, over SQL the claims guard never reads.
    fn carrying(base: &str, result: &str, carries: &str) -> ParsedStep {
        let id = format!(
            "{}-{}.sql",
            &base[..NAME_PREFIX_LEN],
            &result[..NAME_PREFIX_LEN]
        );
        let body = format!("-- carries: {carries}\nSELECT 1;\n");
        parse_step(&id, &step_file_carrying(base, result, &body)).expect("well-formed step")
    }

    fn step_file_carrying(base: &str, result: &str, tail: &str) -> String {
        format!("-- base:   {base}\n-- result: {result}\n{tail}")
    }

    #[test]
    fn a_registered_step_no_test_names_is_refused() {
        let steps = [carrying(A, B, "")];
        let err = inspect_step_coverage(&steps, "// a suite about something else\n")
            .expect_err("named nowhere");
        assert!(err.contains("aaaaaaaa-bbbbbbbb.sql"), "{err}");
        assert!(err.contains("reviewed and never run"), "{err}");

        assert_eq!(
            inspect_step_coverage(&steps, "migrate(pool, \"aaaaaaaa-bbbbbbbb.sql\")"),
            Ok(())
        );

        // The empty registry, which is the shape that makes the question
        // vacuous rather than answered.
        let err = inspect_step_coverage(&[], "anything").expect_err("nothing registered");
        assert!(err.contains("true of nothing"), "{err}");
    }

    #[test]
    fn a_step_that_opens_its_own_transaction_is_refused() {
        // The applier owns one transaction covering the step, the backend
        // migrations, the privileges and the ledger row. A COMMIT inside the
        // step ends it early, and everything after runs unprotected — so a
        // later failure rolls back nothing and still reports a refusal.
        for control in ["BEGIN;", "COMMIT;", "ROLLBACK;", "START TRANSACTION;"] {
            let sql = step_file(A, B, &format!("{control}\nSELECT 1;\n"));
            let err = parse_step("aaaaaaaa-bbbbbbbb.sql", &sql).expect_err("transaction control");
            assert!(
                err.contains("does not own its transaction"),
                "{control}: {err}"
            );
        }
        // `BEGIN` as a word inside plpgsql is not transaction control, and a
        // step full of DO blocks would be unwritable if this refused it.
        let body = "DO $$\nBEGIN\n  PERFORM 1;\nEND $$;\n";
        parse_step("aaaaaaaa-bbbbbbbb.sql", &step_file(A, B, body))
            .expect("a DO block is not a transaction");
    }

    #[test]
    fn a_migration_no_step_claims_is_refused() {
        // The failure this whole mechanism exists for: `BACKEND_MIGRATIONS`
        // runs at initialization and never again, so an unclaimed file reaches
        // every new database and no existing one — and nothing about either
        // database looks wrong.
        let present = vec![
            "0001_kernel_internal".to_owned(),
            "0002_writer".to_owned(),
            "0003_blob".to_owned(),
            "0004_checkpoint".to_owned(),
            "0005_pty_delivery".to_owned(),
        ];
        let steps = [carrying(A, B, "")];
        let err = inspect_backend_migration_claims(&present, &steps).expect_err("0005 unclaimed");
        assert!(err.contains("\"0005_pty_delivery\""), "{err}");
        assert!(err.contains("carried by no step"), "{err}");

        // Claimed, and it passes.
        let steps = [carrying(A, B, "0005_pty_delivery")];
        assert_eq!(inspect_backend_migration_claims(&present, &steps), Ok(()));
    }

    #[test]
    fn a_migration_two_steps_claim_is_refused_and_names_both() {
        let present = vec![
            "0001_kernel_internal".to_owned(),
            "0002_writer".to_owned(),
            "0003_blob".to_owned(),
            "0004_checkpoint".to_owned(),
            "0005_pty_delivery".to_owned(),
        ];
        let steps = [
            carrying(A, B, "0005_pty_delivery"),
            carrying(B, C, "0005_pty_delivery"),
        ];
        let err = inspect_backend_migration_claims(&present, &steps).expect_err("claimed twice");
        assert!(err.contains("aaaaaaaa-bbbbbbbb.sql"), "{err}");
        assert!(err.contains("bbbbbbbb-cccccccc.sql"), "{err}");
        // Applying a migration twice is the failure; the message has to name
        // both claimants, because either one could be the one to edit.
        assert!(err.contains("twice"), "{err}");
    }

    #[test]
    fn a_claim_on_a_file_that_does_not_exist_is_refused() {
        let present = vec!["0001_kernel_internal".to_owned()];
        let steps = [carrying(A, B, "0009_renamed_away")];
        let err = inspect_backend_migration_claims(&present, &steps).expect_err("no such file");
        assert!(err.contains("0009_renamed_away"), "{err}");
        assert!(err.contains("no such file"), "{err}");
    }

    #[test]
    fn an_empty_migration_directory_is_its_own_refusal() {
        // Count before folding. Over zero files every "claimed exactly once"
        // question is vacuously satisfied, and the guard would report a clean
        // set it never looked at.
        let steps = [carrying(A, B, "")];
        let err = inspect_backend_migration_claims(&[], &steps).expect_err("nothing present");
        assert!(err.contains("no backend migrations found"), "{err}");
        assert!(err.contains("every answer is yes"), "{err}");
    }

    #[test]
    fn the_carries_header_is_required_and_bounded() {
        // Absent is not the same as empty. One is a step that carries nothing;
        // the other is a step somebody stopped writing.
        let no_header = format!("-- base:   {A}\n-- result: {B}\nSELECT 1;\n");
        let err = parse_step("aaaaaaaa-bbbbbbbb.sql", &no_header).expect_err("no carries line");
        assert!(err.contains("-- carries:"), "{err}");

        // Empty is legal, and safe because the guard above refuses a file no
        // step claims.
        let empty = carrying(A, B, "");
        assert!(empty.carries.is_empty());

        // Shapes that are not migration stems.
        for bad in ["pty_delivery", "0005-pty-delivery", "0005_PTY", "005_x"] {
            let sql = step_file_carrying(A, B, &format!("-- carries: {bad}\nSELECT 1;\n"));
            let err = parse_step("aaaaaaaa-bbbbbbbb.sql", &sql).expect_err("bad stem");
            assert!(err.contains("NNNN_lower_snake"), "{bad}: {err}");
        }

        // And one step naming the same migration twice.
        let sql = step_file_carrying(A, B, "-- carries: 0005_pty_delivery, 0005_pty_delivery\n");
        let err = parse_step("aaaaaaaa-bbbbbbbb.sql", &sql).expect_err("named twice");
        assert!(err.contains("twice"), "{err}");
    }

    #[test]
    fn state_1_at_the_anchor_an_empty_registry_is_tolerated() {
        // Nothing has moved that a step would have to describe. This also
        // catches the anchor drifting from the file, since a mismatch here
        // takes the refusal branch.
        //
        // When the contract does eventually change, this test reds — and the
        // honest fix is to DELETE it along with the anchor and the branch it
        // covers, because by then the registry is non-empty and neither is ever
        // read again. Refreshing the constant is the one move it must not
        // prompt.
        assert_eq!(inspect_step_chain(&anchored_sql(), &[]), Ok(()));
    }

    #[test]
    fn state_2_at_the_anchor_a_chain_that_arrives_is_allowed() {
        // A step that reproduces historical DDL migrates a database onto the
        // contract this repository already ships, WITHOUT touching
        // `schema/0001_contract.sql` — so the digest stays on the anchor while
        // the registry stops being empty. The anchor must have nothing to say
        // about that; the only question worth asking is whether the chain
        // arrives, and it does.
        let sql = anchored_sql();
        let current = crate::schema::sha256_hex(sql.as_bytes());
        let steps = [parsed(A, &current)];
        assert_eq!(inspect_step_chain(&sql, &steps), Ok(()));
    }

    #[test]
    fn state_3_off_the_anchor_an_empty_registry_names_what_nothing_arrives_at() {
        let moved = format!("{}-- moved\n", anchored_sql());
        let err = inspect_step_chain(&moved, &[]).expect_err("the digest moved");
        assert!(err.contains("no migration step results in"), "{err}");
        // The digest in the message is the one computed HERE. It equals the one
        // `contract_sql.rs` records only when the gate is doing nothing.
        assert!(
            err.contains(&crate::schema::sha256_hex(moved.as_bytes())),
            "{err}"
        );
    }

    #[test]
    fn a_forked_registry_is_refused_before_the_terminal_is_looked_for() {
        // Two ways forward from A. Either branch could be called the terminal,
        // which is the point: the fork has to be refused before anything picks.
        let moved = format!("{}-- moved\n", anchored_sql());
        let err = inspect_step_chain(&moved, &[parsed(A, B), parsed(A, C)])
            .expect_err("the registry forks at A");
        assert!(err.contains("both base on"), "{err}");
        assert!(err.contains(A), "{err}");
    }

    #[test]
    fn a_registry_with_no_terminal_is_refused_by_the_count_not_by_the_first_hit() {
        // A loop: every result is somebody's base, so the filter finds nothing.
        // `find()` would have returned None here, and a gate that read None as
        // "no mismatch" would pass a registry that goes nowhere.
        let moved = format!("{}-- moved\n", anchored_sql());
        let err =
            inspect_step_chain(&moved, &[parsed(A, B), parsed(B, A)]).expect_err("A and B loop");
        assert!(err.contains("expected exactly one terminal step"), "{err}");
        assert!(err.contains("found 0"), "{err}");
    }

    #[test]
    fn state_4_off_the_anchor_a_chain_that_ends_elsewhere_is_refused() {
        let moved = format!("{}-- moved\n", anchored_sql());
        let err = inspect_step_chain(&moved, &[parsed(A, B), parsed(B, C)]).expect_err("ends at C");
        assert!(err.contains("no migration step results in"), "{err}");
        assert!(err.contains("the chain ends at"), "{err}");
        assert!(err.contains("bbbbbbbb-cccccccc.sql"), "{err}");
    }

    #[test]
    fn a_chain_that_ends_at_the_contract_digest_passes() {
        // The only shape that gets through: a line ending exactly where the SQL
        // is now, built by naming the real digest as the last result.
        let moved = format!("{}-- moved\n", anchored_sql());
        let target = crate::schema::sha256_hex(moved.as_bytes());
        let steps = [parsed(A, B), parsed(B, &target)];
        assert_eq!(inspect_step_chain(&moved, &steps), Ok(()));
    }

    #[test]
    fn a_step_no_walk_from_the_contract_reaches_is_refused() {
        // A line plus a closed cycle, and every check before the walk passes
        // over it. No digest is based on twice, so there is no fork. And no
        // member of a cycle is anything's terminal, because each of them is
        // some other member's predecessor — so the line's terminal is the only
        // one and the count agrees. The island's two files read as applicable
        // steps and no database can ever take them.
        let moved = format!("{}-- moved\n", anchored_sql());
        let target = crate::schema::sha256_hex(moved.as_bytes());
        let steps = [parsed(A, &target), parsed(D, E), parsed(E, D)];
        let err = inspect_step_chain(&moved, &steps).expect_err("an island nothing reaches");
        assert!(err.contains("registered and unreachable"), "{err}");
        assert!(err.contains("dddddddd-eeeeeeee.sql"), "{err}");
        assert!(err.contains("eeeeeeee-dddddddd.sql"), "{err}");

        // The same registry with the island removed. This is what makes the
        // refusal above a statement about reachability rather than about the
        // count of steps.
        assert_eq!(inspect_step_chain(&moved, &[parsed(A, &target)]), Ok(()));
    }

    #[test]
    fn two_steps_arriving_at_one_digest_are_refused() {
        // A merge. The fork check cannot see this one: it refuses two steps
        // LEAVING a digest, and these two ARRIVE at one. Bases stay unique, the
        // terminal stays single, the chain still ends where the contract is —
        // and "the step before this one" is a choice.
        let moved = format!("{}-- moved\n", anchored_sql());
        let target = crate::schema::sha256_hex(moved.as_bytes());
        let steps = [parsed(A, C), parsed(B, C), parsed(C, &target)];
        let err = inspect_step_chain(&moved, &steps).expect_err("two steps result in C");
        assert!(err.contains("2 steps result in"), "{err}");
        assert!(err.contains("the chain is a line"), "{err}");
    }

    #[test]
    fn the_transaction_guard_sees_past_the_start_of_a_line() {
        // Three shapes the line-oriented predecessor to this check walked past.
        // Each of them ends the applier's transaction partway through a
        // migration, which is the failure the guard exists for.
        for (label, body) in [
            // PostgreSQL treats `END` as a synonym for COMMIT, and it was left
            // off the keyword list because plpgsql closes every block with it.
            ("a bare END", "END;\nSELECT 1;\n"),
            // Nowhere near the start of a line.
            ("a second statement on one line", "SELECT 1; COMMIT;\n"),
            // Neither `COMMIT;` nor a `COMMIT ` prefix.
            ("an unterminated COMMIT", "SELECT 1;\nCOMMIT\n"),
        ] {
            let sql = step_file(A, B, body);
            let err = parse_step("aaaaaaaa-bbbbbbbb.sql", &sql).expect_err(label);
            assert!(
                err.contains("does not own its transaction"),
                "{label}: {err}"
            );
        }

        // And the shape that has to stay writable, which is why this is a
        // scanner and not a longer keyword list: `END` closing plpgsql blocks,
        // the one place the word is not the statement.
        let body = "DO $$\nBEGIN\n  IF true THEN\n    PERFORM 1;\n  END IF;\nEND $$;\n";
        parse_step("aaaaaaaa-bbbbbbbb.sql", &step_file(A, B, body))
            .expect("plpgsql closes its blocks with END");

        // A comment and a string literal are not statements either. Refusing
        // them would make the guard unusable over the SQL this repository
        // actually writes, which discusses its own transaction rules in prose.
        let body = "-- COMMIT; is discussed here\nSELECT 'COMMIT;' AS note;\n";
        parse_step("aaaaaaaa-bbbbbbbb.sql", &step_file(A, B, body))
            .expect("a comment and a literal are not transaction control");
    }

    #[test]
    fn a_backend_migration_whose_bytes_moved_is_refused() {
        let dir = std::env::temp_dir().join(format!("gwk-frozen-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the fixture dir");

        // Seeded from the REAL files, so the clean arm below passes for the
        // same reason the repository does rather than for a reason this test
        // invented.
        let real = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask has a parent dir")
            .join(MIGRATIONS_DIR);
        let present: Vec<String> = FROZEN_BACKEND_MIGRATIONS
            .iter()
            .map(|(stem, _)| (*stem).to_owned())
            .collect();
        for stem in &present {
            let sql = std::fs::read_to_string(real.join(format!("{stem}.sql")))
                .unwrap_or_else(|err| panic!("read {stem}: {err}"));
            std::fs::write(dir.join(format!("{stem}.sql")), sql).expect("write the fixture");
        }
        assert_eq!(inspect_frozen_backend_migrations(&dir, &present), Ok(()));

        // One line, appended to a file every existing database has already run
        // and nothing will run again.
        let (stem, pin) = FROZEN_BACKEND_MIGRATIONS[0];
        let victim = dir.join(format!("{stem}.sql"));
        let mut sql = std::fs::read_to_string(&victim).expect("read the victim");
        sql.push_str("-- and one more line\n");
        std::fs::write(&victim, sql).expect("write the victim");
        let err = inspect_frozen_backend_migrations(&dir, &present).expect_err("the bytes moved");
        assert!(err.contains("is pinned at"), "{err}");
        assert!(err.contains(pin), "{err}");

        // And a file with no pin, which is one edit away from being a file
        // nothing watches.
        std::fs::write(dir.join("0099_unpinned.sql"), "SELECT 1;\n").expect("write the unpinned");
        let mut with_extra = present.clone();
        with_extra.push("0099_unpinned".to_owned());
        let err =
            inspect_frozen_backend_migrations(&dir, &with_extra).expect_err("one file too many");
        assert!(err.contains("are pinned and"), "{err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_empty_registry_still_emits_the_type_and_the_layout_guard() {
        let out = contract_steps_rs(&[]);
        assert!(out.contains("pub struct Step {"), "{out}");
        assert!(
            out.contains("pub const CONTRACT_STEPS: &[Step] = &[];"),
            "{out}"
        );
        // Dropping this is how the generator and rustfmt start fighting over a
        // file neither of them can win.
        assert!(out.contains("#[rustfmt::skip]"), "{out}");
    }

    #[test]
    fn the_emitted_slice_is_byte_pinned() {
        // Pinned because `cargo fmt --check` cannot police this file — the
        // `#[rustfmt::skip]` above is what keeps rustfmt out of it, so an edit
        // to the emitter has no other guard. Both counts are covered: a
        // one-element slice is where rustfmt's own layout diverges, and it is
        // the shape the repository will carry first.
        let one = parse_step("aaaaaaaa-bbbbbbbb.sql", &step_file(A, B, "SELECT 1;\n"))
            .expect("well-formed step");
        let two = parse_step("bbbbbbbb-cccccccc.sql", &step_file(B, C, "SELECT 2;\n"))
            .expect("well-formed step");

        let rendered = |step: &ParsedStep| {
            format!(
                "    Step {{\n        \
                     id: \"{}\",\n        \
                     base: \"{}\",\n        \
                     result: \"{}\",\n        \
                     backend_migrations: &[],\n        \
                     sql: r#\"{}\"#,\n    \
                 }},\n",
                step.id, step.base, step.result, step.sql
            )
        };

        let out = contract_steps_rs(std::slice::from_ref(&one));
        assert!(
            out.ends_with(&format!(
                "pub const CONTRACT_STEPS: &[Step] = &[\n{}];\n",
                rendered(&one)
            )),
            "{out}"
        );

        let out = contract_steps_rs(&[one.clone(), two.clone()]);
        assert!(
            out.ends_with(&format!(
                "pub const CONTRACT_STEPS: &[Step] = &[\n{}{}];\n",
                rendered(&one),
                rendered(&two)
            )),
            "{out}"
        );
    }
}
