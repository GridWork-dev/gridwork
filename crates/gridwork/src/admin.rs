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
