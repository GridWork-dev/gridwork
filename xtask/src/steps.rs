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
    for (number, line) in sql.lines().enumerate() {
        let statement = line.trim_start().to_ascii_uppercase();
        // `END` is a synonym for COMMIT in PostgreSQL and is deliberately NOT
        // here: plpgsql closes every block with `END;` or `END $$;`, and a step
        // made of DO blocks — which this one is — would be unwritable. A bare
        // plpgsql `BEGIN` is safe to list because it carries no semicolon,
        // which is what distinguishes it from the statement.
        for keyword in ["BEGIN", "COMMIT", "ROLLBACK", "START TRANSACTION"] {
            if statement == format!("{keyword};") || statement.starts_with(&format!("{keyword} ")) {
                return Err(format!(
                    "{id}:{}: carries `{}`, and a step does not own its transaction — the \
                     applier wraps it together with the backend migrations, the privilege \
                     matrix and the ledger row, and a COMMIT here ends that",
                    number + 1,
                    line.trim()
                ));
            }
        }
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

/// The step ids, for a message that would otherwise name a count and nothing to
/// look at.
fn step_ids(steps: &[ParsedStep]) -> String {
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
