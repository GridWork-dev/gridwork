//! The two verbs that hold a database and a key.
//!
//! Everything else `gw` does goes through the socket. `daemon` and `admin` are
//! the only doors to `GWK_DATABASE_URL`, `GWK_ADMIN_DATABASE_URL`, and the blob
//! KEK, and keeping them separate verbs is what makes that statement checkable:
//! a reader can see which code paths touch credentials by reading which module
//! they are in.
//!
//! # What runs where
//!
//! * `daemon` takes the writer lock, recovers, and serves until it is told to
//!   stop or loses write authority. It needs the RUNTIME credential.
//! * `admin init` applies the contract, grants the runtime role, and appends
//!   genesis. It needs the ADMIN credential and takes the writer lock, because
//!   claiming an epoch beside a live kernel would fence that kernel out of its
//!   own log.
//! * `admin verify` and `admin rebuild-projections` read. Neither takes the
//!   lock, and the rebuild opens a READER store on purpose — an ordinary one
//!   claims an epoch, and a verification that deposed the writer it was
//!   verifying would be worse than no verification.
//! * `admin blob` is retention: pin, unpin, sweep, shred. Off the client socket
//!   because no wire request removes a blob, and beside `init` because these are
//!   operator acts on stored bytes rather than questions about them.
//! * `admin blob rotate` is here for a sharper reason than the rest of that
//!   list: it is the only verb that holds TWO keys at once. Reaching it through
//!   the socket would mean handing an incoming KEK to a running daemon, and the
//!   split this module exists to make checkable is that key material has one
//!   door.

use gwk_domain::blob::BlobAddress;
use gwk_domain::ids::EvidenceId;
use gwk_domain::port::BlobStore as _;
use gwk_domain::protocol::KernelErrorCode;
use gwk_kernel::config::{AdminConfig, BlobConfig, KernelConfig};
use gwk_kernel::project::Refusal;
use gwk_kernel::wire::listen::{Listener, notify_ready};
use gwk_kernel::wire::serve::{self, Daemon};
use gwk_kernel::{InitOutcome, PgBlobStore, PgEventStore, TargetState, WriterLock, admin, recover};
use secrecy::{ExposeSecret as _, SecretString};
use serde_json::{Value, json};

use crate::exit::Failure;
use crate::{PUBLIC_REVISION, emit};

/// Connections the daemon's pool may hold.
///
/// Above `MAX_INFLIGHT_APPENDS` because every connection also reads — readiness,
/// pages, a subscription's catch-up — and an append that had to wait for a read
/// to give a connection back would turn the admission bound into a lie.
const POOL_CONNECTIONS: u32 = (gwk_kernel::MAX_INFLIGHT_APPENDS as u32) * 2;

/// What `gw admin blob` was asked to do.
#[derive(Debug, PartialEq, Eq)]
pub enum Retention {
    Pin {
        address: BlobAddress,
        evidence: String,
    },
    Unpin {
        address: BlobAddress,
        evidence: String,
    },
    Sweep,
    Shred {
        address: BlobAddress,
    },
    /// Move every blob off the running KEK and onto `GWK_BLOB_KEK_NEXT`.
    ///
    /// No address: rotation is a property of the store, and a per-blob rotation
    /// would leave a deployment whose blobs are split across two keys with
    /// nothing recording which is which.
    Rotate,
}

/// Serve until told to stop.
pub async fn daemon(pretty: bool) -> Result<(), Failure> {
    // First, because it is the only thing here that is worth refusing before any
    // connection is made: a daemon that cannot say which build it is makes the
    // revision genesis recorded uncomparable.
    let revision = revision()?;
    let config = KernelConfig::from_env().map_err(configuration)?;
    let blob_config = BlobConfig::from_env().map_err(configuration)?;

    // The lock before the pool. Another live kernel means this one must not
    // start, and `acquire` never waits — blocking would hide a second writer
    // behind a hang.
    let lock = WriterLock::acquire(config.database_url())
        .await
        .map_err(|e| Failure::new(KernelErrorCode::Fenced, e.to_string()))?;

    // One connection per in-flight append, plus headroom for the reads a
    // connection makes alongside them.
    let pool = gwk_kernel::connect_pool(config.database_url(), POOL_CONNECTIONS)
        .await
        .map_err(configuration)?;
    // The credential is checked before the socket exists. A daemon that could
    // rewrite history is not one to start and then complain about.
    let privileges = admin::runtime_privileges(&pool)
        .await
        .map_err(configuration)?;
    let violations = privileges.violations();
    if !violations.is_empty() {
        return Err(Failure::new(
            KernelErrorCode::Privilege,
            format!(
                "this credential holds privileges the kernel refuses to run with: {}",
                violations.join(", ")
            ),
        ));
    }

    let blobs = PgBlobStore::open(pool.clone(), blob_config)
        .await
        .map_err(blob_failure)?;
    let store = PgEventStore::open(pool)
        .await
        .map_err(configuration)?
        .with_blobs(blobs);

    // Recovery before the socket, because readiness is a claim about the
    // projections and this is what establishes what may be claimed.
    let recovered = store.recover().await.map_err(refusal)?;
    if !recovered.ready() {
        return Err(Failure::new(
            KernelErrorCode::Storage,
            format!(
                "the projections do not agree with the log ({:?}); refusing to serve",
                recovered.verdict
            ),
        ));
    }

    let daemon =
        std::sync::Arc::new(Daemon::new(store, revision.to_owned()).map_err(configuration)?);
    let listener = Listener::bind(config.socket_path())
        .await
        .map_err(configuration)?;

    emit(
        &json!({
            "type": "daemon_started",
            "socket_path": config.socket_path().to_string_lossy(),
            "public_revision": revision,
            "watermark": recovered.watermark.map(|seq| seq.value().to_string()),
            "verdict": verdict(&recovered),
            // Surfaced rather than swallowed: a checkpoint that failed
            // validation will keep failing, and silence lets it.
            "rejected_checkpoints": recovered.rejected.len(),
            "uncertain_attempts": recovered.uncertain,
            "notified_systemd": notify_ready(),
        }),
        pretty,
    );

    // Three ways to stop, and the third is the one that matters: losing the
    // advisory lock means another process took write authority, so this one
    // stops accepting rather than racing it. The lock is MOVED in here so it
    // lives exactly as long as the service does.
    let stopped = serve::run(listener, daemon, async move {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            () = terminated() => {}
            () = lock.cancelled() => {}
        }
    })
    .await
    .map_err(configuration)?;

    emit(
        &json!({
            "type": "daemon_stopped",
            // The parting snapshot's sequence, which is what lets the next start
            // answer `verified` instead of `unverified`. Null with a reason is a
            // barrier that stopped firing — reported, because the alternative is
            // discovering it at a restart that replays the whole log.
            "checkpoint": stopped.checkpoint.map(|seq| seq.value().to_string()),
            "checkpoint_error": stopped.checkpoint_error,
        }),
        pretty,
    );
    Ok(())
}

/// `SIGTERM`, which is how a service manager asks.
async fn terminated() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) {
        Ok(mut term) => {
            term.recv().await;
        }
        // Nothing to listen on: fall back to never resolving, so the other two
        // arms still decide. A daemon that exited because it could not install a
        // handler would be worse than one that only answers Ctrl-C.
        Err(_) => std::future::pending().await,
    }
}

/// Apply the contract, grant the runtime role, and append genesis.
pub async fn init(pretty: bool) -> Result<(), Failure> {
    let revision = revision()?;
    let config = AdminConfig::from_env().map_err(configuration)?;
    // Same reason as the daemon's: `PgEventStore::open` claims an epoch, and
    // claiming one beside a running kernel fences that kernel out of its own
    // log. Initialization is a one-shot against an empty database, so refusing
    // while anything else holds write authority costs nothing.
    let _lock = WriterLock::acquire(config.admin_database_url())
        .await
        .map_err(|e| Failure::new(KernelErrorCode::Fenced, e.to_string()))?;
    let pool = gwk_kernel::connect_pool(config.admin_database_url(), 4)
        .await
        .map_err(configuration)?;

    let outcome = admin::init(&pool, &config).await.map_err(configuration)?;
    let store = PgEventStore::open(pool).await.map_err(configuration)?;
    // Idempotent by the same key genesis has always used, so re-running this
    // against an initialized database is a no-op rather than a second epoch.
    store.ensure_genesis(&revision).await.map_err(refusal)?;

    emit(
        &json!({
            "type": "admin_initialized",
            "outcome": match outcome {
                InitOutcome::Initialized => "initialized",
                InitOutcome::AlreadyInitialized => "already_initialized",
            },
            "runtime_role": config.runtime_role(),
            "public_revision": revision,
            "contract_sha256": gwk_kernel::CONTRACT_SQL_SHA256,
        }),
        pretty,
    );
    Ok(())
}

/// Carry this database from the contract it records to the one this binary
/// carries.
///
/// The order of the first three acts is the whole of the operator-facing
/// design, and each is before the next because of what going second would cost:
///
/// 1. **The backup is read and digested first**, before any lock. A verb that
///    fences a live daemon and then complains about a typo in a path has taken
///    something away for nothing. This never shells out to `pg_dump` — the
///    major-version judgement belongs to the operator, who is the only party
///    who knows which `pg_dump` matches the server.
/// 2. **The writer lock**, exactly as `init` takes it, before the pool. It
///    never waits, so a running kernel is a refusal rather than a hang.
/// 3. **The chain is resolved** against what the database records, and the
///    refusal — when there is one — carries the recorded digest, this binary's,
///    and every base the registry knows, because "no chain" without the
///    candidates is a sentence an operator cannot act on.
///
/// `--from` asserts a base rather than reading one. It is the one path where
/// the precondition goes unchecked, so both the receipt and the ledger row
/// carry `asserted_base: true` alongside the digest that was actually recorded
/// at the time — the row is the only thing that will ever say so. It relaxes
/// the assertion, not the chain: a `--from` naming a digest no step bases on is
/// still refused.
pub async fn migrate(
    scratch: &str,
    backup: Option<&str>,
    from: Option<&str>,
    dry_run: bool,
    pretty: bool,
) -> Result<(), Failure> {
    // FIRST, before the revision stamp and before anything reads the
    // environment. It is the cheapest check in the verb and it catches the
    // most common mistake, and every statement after it costs the operator
    // something to undo.
    // See the doc above.
    let backup_sha256 = match backup {
        Some(path) => Some(digest_backup(path)?),
        None => {
            if !dry_run {
                // Named, not merely declined: the operator who passed this flag
                // has to be told what it costs while it still costs nothing.
                emit(
                    &json!({
                        "type": "admin_migrate_unbacked",
                        "warning": concat!(
                            "--no-backup: this migration will have no restore path. ",
                            "If the chain commits and the result is wrong there is ",
                            "nothing to restore from — the log is intact but the ",
                            "schema is not the one the old binaries serve",
                        ),
                    }),
                    pretty,
                );
            }
            None
        }
    };

    let revision = revision()?;
    let config = AdminConfig::from_env().map_err(configuration)?;
    let _lock = WriterLock::acquire(config.admin_database_url())
        .await
        .map_err(|e| Failure::new(KernelErrorCode::Fenced, e.to_string()))?;
    let pool = gwk_kernel::connect_pool(config.admin_database_url(), 4)
        .await
        .map_err(configuration)?;

    let recorded = match admin::inspect(&pool).await.map_err(configuration)? {
        TargetState::Initialized { contract_sha256 } => contract_sha256,
        other => {
            return Err(Failure::new(
                KernelErrorCode::Schema,
                format!(
                    "this database is {}, and a migration carries a database that already                      records a contract",
                    match other {
                        TargetState::Empty => "empty — `gw admin init` is the verb for that",
                        _ => "not one this binary recognizes",
                    }
                ),
            ));
        }
    };

    let base = from.unwrap_or(recorded.as_str());
    let asserted_base = from.is_some();
    let chain = gwk_kernel::migrate::resolve(
        gwk_kernel::CONTRACT_STEPS,
        base,
        gwk_kernel::CONTRACT_SQL_SHA256,
    )
    .map_err(|refusal| Failure::new(KernelErrorCode::Schema, refusal.to_string()))?;

    let steps: Vec<&str> = chain.iter().map(|step| step.id).collect();
    let carried: Vec<&str> = chain
        .iter()
        .flat_map(|step| step.backend_migrations.iter().copied())
        .collect();

    // R1, before any statement executes and before the dry run reports a plan
    // it could not carry out. A chain resolved from a base the database is not
    // at describes a shape that is not there.
    gwk_kernel::migrate::assert_base(&pool, base)
        .await
        .map_err(configuration)?;

    if dry_run {
        // R3 against the live database, which is the rung a dry run CAN
        // answer: the grant matrix is a property of the schema as it stands,
        // and reading it changes nothing. R2's rehearsal is the rung a dry run
        // is really for and it is not here yet — see the report.
        gwk_kernel::migrate::assert_grant_matrix(&pool, config.runtime_role())
            .await
            .map_err(configuration)?;

        emit(
            &json!({
                "type": "admin_migrate_planned",
                "scratch_database": scratch,
                "recorded_sha256": recorded,
                "base_sha256": base,
                "asserted_base": asserted_base,
                "contract_sha256": gwk_kernel::CONTRACT_SQL_SHA256,
                "steps": steps,
                "backend_migrations": carried,
                "backup_sha256": backup_sha256,
                "public_revision": revision,
                "grant_matrix": "checked",
            }),
            pretty,
        );
        return Ok(());
    }

    let applied = gwk_kernel::migrate::apply(
        &pool,
        &chain,
        config.runtime_role(),
        &recorded,
        asserted_base,
        &revision,
        backup_sha256.as_deref(),
    )
    .await
    .map_err(configuration)?;

    // The rungs that can only be asked after the transaction commits. R5 reads
    // the fingerprint and the log again rather than inferring either from the
    // absence of an error; R3 is re-asked because the step changed the table
    // set, and `GRANT ... ON ALL TABLES` reaches the relations that existed
    // when it ran. R4 proves the guards still refuse a superuser — the credential
    // no grant binds, which is the one that just applied a migration.
    gwk_kernel::migrate::assert_result(&pool, &applied)
        .await
        .map_err(configuration)?;
    gwk_kernel::migrate::assert_grant_matrix(&pool, config.runtime_role())
        .await
        .map_err(configuration)?;
    gwk_kernel::migrate::assert_protections(&pool)
        .await
        .map_err(configuration)?;

    emit(
        &migrated_receipt(
            scratch,
            &recorded,
            base,
            asserted_base,
            &applied,
            backup_sha256.as_deref(),
            &revision,
        ),
        pretty,
    );
    Ok(())
}

/// The receipt one completed migration leaves behind.
///
/// A function rather than an inline `json!` at the `emit` call site, so its
/// field set can be asserted without capturing stdout. That is not a testing
/// convenience: this document is the only artifact that will ever say what a
/// live migration did, and a field that quietly stops being emitted takes its
/// evidence with it.
///
/// Thirteen keys. The SPEC's criterion 8 names eight of them; the other five
/// are here because the operator reading this afterwards needs them just as
/// much — which database was rehearsed against, what the fingerprint said
/// BEFORE the run, and whether the base was asserted rather than read.
fn migrated_receipt(
    scratch: &str,
    recorded: &str,
    base: &str,
    asserted_base: bool,
    applied: &gwk_kernel::migrate::Applied,
    backup_sha256: Option<&str>,
    revision: &str,
) -> Value {
    json!({
        "type": "admin_migrated",
        "scratch_database": scratch,
        // What the database said before the run, and what the chain was
        // resolved from. They differ exactly when `--from` overrode the base,
        // which is why both are here rather than one.
        "recorded_sha256": recorded,
        "base_sha256": base,
        "asserted_base": asserted_base,
        "contract_sha256": applied.result,
        "steps": applied.steps,
        // The step's work and the backend migrations' work, kept apart: the
        // contract DDL never mentions the relations these create.
        "backend_migrations": applied.backend_migrations,
        "backup_sha256": backup_sha256,
        "public_revision": revision,
        // The chain writes no events. A difference between these two is a
        // daemon that was not fenced, and the receipt is where that shows.
        "events_before": applied.events_before,
        "events_after": applied.events_after,
        "elapsed_ms": applied.elapsed_ms,
    })
}

/// SHA-256 of the backup file, computed here.
///
/// Computed rather than accepted: a digest the operator supplies is a digest of
/// whatever they digested, and the receipt's claim is about the file this verb
/// could actually read at the moment it ran.
fn digest_backup(path: &str) -> Result<String, Failure> {
    use sha2::{Digest, Sha256};

    let mut file = std::fs::File::open(path).map_err(|err| {
        Failure::new(
            KernelErrorCode::Storage,
            format!(
                "--backup {path:?} cannot be read: {err}. Checked before the writer lock is                  taken, so nothing has been fenced and nothing has been applied"
            ),
        )
    })?;
    // Streamed in fixed-size blocks rather than read whole: a database dump is
    // exactly the file that does not fit in memory, and the digest is the only
    // thing wanted out of it.
    let mut hasher = Sha256::new();
    let mut block = vec![0u8; 1 << 16];
    loop {
        let read = std::io::Read::read(&mut file, &mut block).map_err(|err| {
            Failure::new(
                KernelErrorCode::Storage,
                format!("--backup {path:?} could not be read to the end: {err}"),
            )
        })?;
        if read == 0 {
            break;
        }
        hasher.update(&block[..read]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut acc, byte| {
            use std::fmt::Write as _;
            let _ = write!(acc, "{byte:02x}");
            acc
        }))
}

/// Read the machine half of a driving-window receipt.
///
/// Lockless like `verify`, because it only reads — a receipt read that could
/// fence the daemon writing the rows it reads would be a strange instrument.
/// Off the client socket because the figures are SQL aggregations over the
/// log, which the wire grammar has no reason to grow.
pub async fn receipt(from: &str, to: Option<&str>, pretty: bool) -> Result<(), Failure> {
    let config = AdminConfig::from_env().map_err(configuration)?;
    let pool = gwk_kernel::connect_pool(config.admin_database_url(), 2)
        .await
        .map_err(configuration)?;

    // Resolution failures are input failures — the resolve query does
    // nothing but cast the caller's two values — so they exit as usage, not
    // as the retryable storage class a wrapper would loop on.
    let window = admin::resolve_window(&pool, from, to)
        .await
        .map_err(|error| Failure::usage(format!("the window did not resolve: {error}")))?;
    let figures = admin::driving_figures(&pool, &window)
        .await
        .map_err(configuration)?;
    emit(
        &json!({
            "type": "receipt_figures",
            // Both bounds as the database resolved them, in one format: a
            // relative input like `yesterday` leaves as the instant it
            // meant, which is what makes the receipt reproducible.
            "window_start": window.start,
            "window_end": window.end,
            // The counts first, so the zeros below are legible: each fold's
            // own denominator sits beside it, and zeros beside a zero
            // denominator mean an empty window, not a quiet one.
            "sessions_in_window": figures.sessions_in_window,
            "sessions_alive_in_window": figures.sessions_alive_in_window,
            "days_driven": figures.days_driven,
            "peak_concurrent": figures.peak_concurrent,
            "generations": figures.generations,
            "restarts": figures.restarts,
            "crashed_generations": figures.crashed_generations,
            "attaches": figures.attaches,
            "detaches": figures.detaches,
        }),
        pretty,
    );
    Ok(())
}

/// Say what the target database is and whether the runtime role is safe.
pub async fn verify(pretty: bool) -> Result<(), Failure> {
    let config = AdminConfig::from_env().map_err(configuration)?;
    let pool = gwk_kernel::connect_pool(config.admin_database_url(), 2)
        .await
        .map_err(configuration)?;

    let state = admin::inspect(&pool).await.map_err(configuration)?;
    let attributes = admin::role_attributes(&pool, config.runtime_role())
        .await
        .map_err(configuration)?;
    let violations: Vec<&'static str> = attributes
        .map(|attributes| attributes.violations())
        .unwrap_or_default();

    let (target, detail) = match &state {
        TargetState::Empty => ("empty", Value::Null),
        TargetState::Initialized { contract_sha256 } => {
            ("initialized", json!({"contract_sha256": contract_sha256}))
        }
        TargetState::Foreign { objects } => ("foreign", json!({"objects": objects})),
    };
    emit(
        &json!({
            "type": "admin_verified",
            "target": target,
            "detail": detail,
            "runtime_role": config.runtime_role(),
            // Absent is not the same as clean: a role that does not exist has no
            // violations and also cannot be granted to.
            "runtime_role_exists": attributes.is_some(),
            "violations": violations,
            "expected_contract_sha256": gwk_kernel::CONTRACT_SQL_SHA256,
        }),
        pretty,
    );

    if !violations.is_empty() {
        return Err(Failure::new(
            KernelErrorCode::Privilege,
            format!("the runtime role holds {}", violations.join(", ")),
        ));
    }
    // A contract that is not THIS contract is a mismatch a caller must not read
    // as agreement, so it exits as one.
    if let TargetState::Initialized { contract_sha256 } = &state
        && contract_sha256 != gwk_kernel::CONTRACT_SQL_SHA256
    {
        return Err(Failure::new(
            KernelErrorCode::Schema,
            format!(
                "the database carries contract {contract_sha256}, and this build is {}",
                gwk_kernel::CONTRACT_SQL_SHA256
            ),
        ));
    }
    Ok(())
}

/// Replay the log into a scratch database and report whether it agrees.
///
/// Nothing is swapped. Replacing the live projections is an operator act with
/// its own downtime, and a comparison that did it as a side effect would be a
/// trap — so this prints a verdict and stops.
pub async fn rebuild_projections(scratch: &str, pretty: bool) -> Result<(), Failure> {
    let config = AdminConfig::from_env().map_err(configuration)?;
    let live = gwk_kernel::connect_pool(config.admin_database_url(), 4)
        .await
        .map_err(configuration)?;

    let scratch_url = beside(config.admin_database_url(), scratch)?;
    let scratch_pool = gwk_kernel::connect_pool(&scratch_url, 4)
        .await
        .map_err(configuration)?;
    // The scratch needs the same contract the replay writes through, so it is
    // initialized like any other target. Already-initialized is the ordinary
    // case on a second run.
    let scratch_config = AdminConfig::from_lookup({
        let url = scratch_url.expose_secret().to_owned();
        let role = config.runtime_role().to_owned();
        move |key| match key {
            gwk_kernel::config::ADMIN_DATABASE_URL_ENV => Some(url.clone()),
            gwk_kernel::config::RUNTIME_ROLE_ENV => Some(role.clone()),
            _ => None,
        }
    })
    .map_err(configuration)?;
    admin::init(&scratch_pool, &scratch_config)
        .await
        .map_err(configuration)?;

    // A READER: an ordinary store claims an epoch, and this is meant to run
    // against a kernel that is still serving.
    let report = PgEventStore::open_reader(live)
        .rebuild_into(&scratch_pool)
        .await
        .map_err(refusal)?;

    emit(
        &json!({
            "type": "projections_rebuilt",
            "scratch_database": scratch,
            "through_sequence": report.through_sequence.map(|seq| seq.value().to_string()),
            "live_hash": report.live_hash,
            "rebuilt_hash": report.rebuilt_hash,
            "agrees": report.agrees,
        }),
        pretty,
    );
    if !report.agrees {
        return Err(Failure::new(
            KernelErrorCode::Storage,
            "the rebuilt projections do not agree with the live ones",
        ));
    }
    Ok(())
}

/// Pin, unpin, sweep, or shred.
pub async fn retention(what: &Retention, pretty: bool) -> Result<(), Failure> {
    let config = AdminConfig::from_env().map_err(configuration)?;
    let blob_config = BlobConfig::from_env().map_err(configuration)?;
    let pool = gwk_kernel::connect_pool(config.admin_database_url(), 4)
        .await
        .map_err(configuration)?;
    let blobs = PgBlobStore::open(pool, blob_config)
        .await
        .map_err(blob_failure)?;

    let answer = match what {
        Retention::Pin { address, evidence } => {
            blobs
                .pin(address, &EvidenceId::new(evidence.clone()))
                .await
                .map_err(blob_failure)?;
            json!({"type": "blob_pinned", "address": address.as_str(), "evidence": evidence})
        }
        Retention::Unpin { address, evidence } => {
            blobs
                .unpin(address, &EvidenceId::new(evidence.clone()))
                .await
                .map_err(blob_failure)?;
            json!({"type": "blob_unpinned", "address": address.as_str(), "evidence": evidence})
        }
        Retention::Sweep => {
            let removed = blobs.sweep().await.map_err(blob_failure)?;
            let addresses: Vec<&str> = removed.iter().map(BlobAddress::as_str).collect();
            json!({"type": "blobs_swept", "removed": addresses})
        }
        Retention::Shred { address } => {
            // Crypto-shred: the wrapped key goes first, so a crash mid-shred
            // leaves an unreadable blob and never a readable one.
            blobs.shred(address).await.map_err(blob_failure)?;
            json!({"type": "blob_shredded", "address": address.as_str()})
        }
        Retention::Rotate => {
            let next = BlobConfig::next_kek_from_env().map_err(configuration)?;
            let report = blobs
                .rewrap_all(next.expose_secret())
                .await
                .map_err(blob_failure)?;
            // Both counts, because `rewrapped: 0` alone reads as "nothing
            // happened" when on a resumed rotation it means "everything was
            // already done" — the one question an operator finishing an
            // interrupted rotation is actually asking.
            json!({
                "type": "blobs_rotated",
                "kek_id": blobs.config().kek_id(),
                "rewrapped": report.rewrapped,
                "already_rotated": report.already,
            })
        }
    };
    emit(&answer, pretty);
    Ok(())
}

/// The revision this process will record and report.
///
/// The build's own stamp if it has one, and only otherwise the environment. That
/// order matters: a stamped build states a fact about the bytes that were
/// compiled and nothing may override it, while an unstamped one has no fact to
/// state, so an operator supplying the revision deliberately is better than a
/// binary that cannot run at all. Neither path invents a value — the third
/// outcome is a refusal.
fn revision() -> Result<String, Failure> {
    if let Some(stamped) = PUBLIC_REVISION {
        return Ok(stamped.to_owned());
    }
    let supplied = std::env::var(REVISION_ENV).ok().filter(|value| {
        value.len() == 40
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    });
    supplied.ok_or_else(|| {
        Failure::usage(format!(
            "this build carries no public revision, so it cannot record or report which build it \
             is; rebuild from a clean checkout or set {REVISION_ENV} to a 40-character lowercase \
             hexadecimal revision"
        ))
    })
}

/// Where an unstamped build may be told its revision. The same name `build.rs`
/// reads, because it is the same fact arriving later.
const REVISION_ENV: &str = "GWK_PUBLIC_REVISION";

/// The same server, a different database. Derived rather than asked for
/// separately so a scratch cannot be pointed at another host by accident.
fn beside(url: &SecretString, database: &str) -> Result<SecretString, Failure> {
    if database.is_empty()
        || !database
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(Failure::usage(format!(
            "{database:?} is not a database name"
        )));
    }
    let (prefix, tail) = url
        .expose_secret()
        .rsplit_once('/')
        .ok_or_else(|| Failure::usage("the admin DSN has no /database to replace"))?;
    // The database name is followed by the query string, if there is one, and
    // dropping it would connect the scratch on different terms than the live
    // one — a DSN that asked for `sslmode=require` would silently stop.
    let query = tail.find('?').map(|at| &tail[at..]).unwrap_or("");
    Ok(SecretString::from(format!("{prefix}/{database}{query}")))
}

/// A configuration or storage error, which is what almost everything here is.
fn configuration(error: gwk_kernel::KernelError) -> Failure {
    Failure::new(KernelErrorCode::Storage, error.to_string())
}

fn refusal(refusal: Refusal) -> Failure {
    Failure::new(refusal.code, refusal.message)
}

fn blob_failure(error: gwk_domain::port::BlobError) -> Failure {
    Failure::new(
        match &error {
            gwk_domain::port::BlobError::NotFound => KernelErrorCode::NotFound,
            gwk_domain::port::BlobError::Tombstoned => KernelErrorCode::BlobTombstoned,
            gwk_domain::port::BlobError::DigestMismatch { .. }
            | gwk_domain::port::BlobError::Integrity(_) => KernelErrorCode::BlobIntegrity,
            // The one code an operator will actually meet here: sweep and shred
            // both refuse a blob that is pinned as evidence.
            gwk_domain::port::BlobError::Pinned => KernelErrorCode::Authority,
            gwk_domain::port::BlobError::Storage(_) => KernelErrorCode::Storage,
        },
        error.to_string(),
    )
}

/// The verdict, as a value rather than a debug rendering.
fn verdict(report: &recover::RecoveryReport) -> Value {
    match &report.verdict {
        recover::Verdict::Verified { anchor } => {
            json!({"verdict": "verified", "anchor": anchor.value().to_string()})
        }
        recover::Verdict::Replayed { events } => {
            json!({"verdict": "replayed", "events": events})
        }
        recover::Verdict::Unverified { reason } => {
            json!({"verdict": "unverified", "reason": reason})
        }
        recover::Verdict::Diverged { expected, found } => {
            json!({"verdict": "diverged", "expected": expected, "found": found})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every key the migrate receipt carries, and no other.
    ///
    /// The whole set, not the eight the SPEC names. A field nobody asserts is
    /// a field that can stop being emitted between one release and the next,
    /// and this receipt is the only artifact that will ever say what a live
    /// migration did to a database nobody can re-run the migration against.
    const RECEIPT_KEYS: [&str; 13] = [
        "type",
        "scratch_database",
        "recorded_sha256",
        "base_sha256",
        "asserted_base",
        "contract_sha256",
        "steps",
        "backend_migrations",
        "backup_sha256",
        "public_revision",
        "events_before",
        "events_after",
        "elapsed_ms",
    ];

    #[test]
    fn the_migrate_receipt_carries_exactly_the_keys_it_promises() {
        let applied = gwk_kernel::migrate::Applied {
            base: "a".repeat(64),
            result: "b".repeat(64),
            steps: vec!["aaaaaaaa-bbbbbbbb.sql".to_owned()],
            backend_migrations: vec!["0005_pty_delivery".to_owned()],
            events_before: 41,
            events_after: 41,
            elapsed_ms: 12,
        };
        let receipt = migrated_receipt(
            "probe",
            &"a".repeat(64),
            &"a".repeat(64),
            false,
            &applied,
            Some(&"c".repeat(64)),
            "0000000000000000000000000000000000000000",
        );
        let object = receipt.as_object().expect("the receipt is an object");

        // COUNT first, and it is not the same assertion as the membership
        // sweep below. A renamed field keeps the count and fails membership; a
        // dropped one fails the count, and the sweep that follows would have
        // reported only the one key it happened to look for first. Neither
        // arm alone distinguishes the two.
        assert_eq!(
            object.len(),
            RECEIPT_KEYS.len(),
            "the receipt carries {} keys, not {}: {:?}",
            object.len(),
            RECEIPT_KEYS.len(),
            object.keys().collect::<Vec<_>>()
        );
        for key in RECEIPT_KEYS {
            assert!(
                object.contains_key(key),
                "the receipt no longer carries {key:?}: {:?}",
                object.keys().collect::<Vec<_>>()
            );
        }

        // And the two digests that differ only under `--from` are both really
        // there, rather than one value emitted twice under two names.
        assert_eq!(object["type"], "admin_migrated");
        assert_eq!(object["asserted_base"], false);
        assert_eq!(object["events_before"], object["events_after"]);
        assert_eq!(object["contract_sha256"], "b".repeat(64));
    }

    /// RED 2: a backup path that does not exist refuses BEFORE anything is
    /// taken away.
    ///
    /// The ordering is the assertion, and it is testable without a database
    /// precisely because of the ordering: this call reaches the backup check
    /// before it reads a credential, resolves a DSN, or takes the writer lock,
    /// so it fails the same way on a machine with no PostgreSQL at all. Move
    /// the check down and this test stops being about the backup — it starts
    /// reporting whatever the environment happens to be missing.
    ///
    /// A verb that fences a live daemon and then complains about a typo has
    /// taken the kernel's write authority away for nothing.
    #[tokio::test]
    async fn a_missing_backup_refuses_before_the_writer_lock() {
        let failure = migrate(
            "probe",
            Some("/nonexistent/gwk-migrate-backup-that-cannot-be-there.dump"),
            None,
            false,
            false,
        )
        .await
        .expect_err("a backup path that does not exist");

        assert_eq!(failure.code, KernelErrorCode::Storage, "{failure:?}");
        assert!(failure.message.contains("--backup"), "{failure:?}");
        assert!(
            failure.message.contains("before the writer lock"),
            "the refusal does not say what it protected: {failure:?}"
        );
        // And specifically NOT a configuration failure: that is what this
        // reports the moment the check moves below `AdminConfig::from_env`.
        assert!(
            !failure.message.contains("GWK_ADMIN_DATABASE_URL"),
            "the backup check ran after the credential was read: {failure:?}"
        );
    }

    #[test]
    fn a_scratch_database_lives_beside_the_one_it_verifies() {
        let url = SecretString::from("postgres://gw@localhost:5432/gwk_live".to_owned());
        let scratch = beside(&url, "gwk_scratch").expect("derive");
        // Same host, same credential, one name changed. Asking for the scratch
        // DSN separately would let a rebuild compare against another server.
        assert_eq!(
            scratch.expose_secret(),
            "postgres://gw@localhost:5432/gwk_scratch"
        );
    }

    #[test]
    fn the_scratch_connects_on_the_same_terms_as_the_live_one() {
        let url = SecretString::from(
            "postgres://gw@localhost:5432/gwk_live?sslmode=require&connect_timeout=5".to_owned(),
        );
        // A rebuild that dropped `sslmode=require` would compare a TLS
        // connection's log against a plaintext one's projections, and the first
        // symptom would be a refused connection on a host that requires it.
        assert_eq!(
            beside(&url, "gwk_scratch").expect("derive").expose_secret(),
            "postgres://gw@localhost:5432/gwk_scratch?sslmode=require&connect_timeout=5"
        );
    }

    #[test]
    fn a_scratch_name_that_is_not_a_name_is_refused() {
        let url = SecretString::from("postgres://gw@localhost:5432/gwk_live".to_owned());
        // Each of these would otherwise be pasted into a DSN, which is a place a
        // caller-supplied string has no business being unchecked.
        for name in ["", "gwk scratch", "gwk;drop", "other/db", "gwk-scratch"] {
            assert_eq!(
                beside(&url, name).expect_err(name).exit,
                crate::exit::USAGE,
                "accepted {name:?}"
            );
        }
    }
}
