//! Singleton write authority: the advisory boot lock and the durable epoch.
//!
//! Two independent guards, because they fail in different directions.
//!
//! The **advisory lock** is held for the process lifetime on its own dedicated
//! connection, taken with the NONBLOCKING variant so a second kernel exits
//! immediately with a clear message instead of hanging on a queue nobody is
//! watching. PostgreSQL releases a session advisory lock when its session ends,
//! so the lock is exactly as alive as that connection — which is why losing the
//! connection and losing the lock are the same event, and why a dead probe is
//! sufficient evidence of both.
//!
//! The **durable epoch** covers what the lock cannot. A process that was fenced
//! out may still hold pooled connections with in-flight work; the lock says
//! nothing about those. Bumping a row at boot and comparing it inside every
//! mutating transaction makes such a write fail at COMMIT time rather than
//! land behind its successor's back.

use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use sqlx::{Connection, Executor, PgConnection, PgPool, Row};
use tokio::sync::watch;

use crate::error::{KernelError, Result};

/// The advisory-lock key for "this database's kernel writer".
///
/// Advisory locks share one namespace per database, so the key is a constant
/// anything else in this database must avoid. It spells `gwk` in the high bytes
/// and `wr1` in the low ones, which makes an unexpected holder identifiable in
/// `pg_locks` instead of anonymous.
pub const WRITER_LOCK_KEY: i64 = 0x0067_776b_0077_7231;

/// How often the holder proves its connection is still alive.
pub const LOCK_PROBE_INTERVAL: Duration = Duration::from_secs(5);

/// The process-lifetime advisory lock, plus the signal that it is gone.
///
/// Dropping this aborts the probe and closes the connection, which releases the
/// lock — so scope IS lifetime, and there is no "forgot to unlock" path.
#[derive(Debug)]
pub struct WriterLock {
    cancelled: watch::Receiver<bool>,
    probe: tokio::task::JoinHandle<()>,
}

impl WriterLock {
    /// Take the lock, or refuse. Never waits: another live kernel means this
    /// one must not start, and blocking would hide that behind a hang.
    pub async fn acquire(database_url: &SecretString) -> Result<Self> {
        Self::acquire_with_interval(database_url, LOCK_PROBE_INTERVAL).await
    }

    pub async fn acquire_with_interval(
        database_url: &SecretString,
        probe_interval: Duration,
    ) -> Result<Self> {
        let mut conn = PgConnection::connect(database_url.expose_secret()).await?;
        let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(WRITER_LOCK_KEY)
            .fetch_one(&mut conn)
            .await?;
        if !acquired {
            return Err(KernelError::Writer(
                "another kernel already holds the writer lock on this database; \
                 exactly one process may write per store"
                    .to_owned(),
            ));
        }

        let (tx, cancelled) = watch::channel(false);
        let probe = tokio::spawn(async move {
            loop {
                tokio::time::sleep(probe_interval).await;
                // A session advisory lock dies with its session, so a failed
                // round trip on THIS connection is proof the lock is gone.
                // Nothing here tries to re-acquire: a kernel that lost write
                // authority must stop, not race the process that took it.
                if conn.execute("SELECT 1").await.is_err() {
                    let _ = tx.send(true);
                    return;
                }
                if tx.is_closed() {
                    return;
                }
            }
        });

        Ok(Self { cancelled, probe })
    }

    /// True once write authority is gone. Callers stop accepting work.
    pub fn is_cancelled(&self) -> bool {
        *self.cancelled.borrow()
    }

    /// Resolves when write authority is lost. Never resolves while healthy, so
    /// it composes in a `select!` beside the accept loop.
    pub async fn cancelled(&self) {
        let mut rx = self.cancelled.clone();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                return; // the probe is gone: treat that as lost authority too
            }
        }
    }
}

impl Drop for WriterLock {
    fn drop(&mut self) {
        self.probe.abort();
    }
}

/// Claim this boot's writer epoch: bump the durable counter under a row lock
/// and return what this process must present from now on.
///
/// Every later mutating transaction compares against this value, so a process
/// that booted before another one finds its epoch superseded and cannot commit.
///
/// No explicit `FOR UPDATE`: an `UPDATE` already takes a row lock, and under
/// READ COMMITTED a blocked one re-evaluates against the committed row rather
/// than its original snapshot — so simultaneous boots serialize and cannot be
/// handed the same number. Verified against 16 with 40 concurrent claims, which
/// returned exactly 1..40.
pub async fn claim_epoch(pool: &PgPool) -> Result<i64> {
    let row = sqlx::query(
        "UPDATE gwk_internal.writer SET epoch = epoch + 1 WHERE id = 1 RETURNING epoch",
    )
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| {
        KernelError::Schema(
            "gwk_internal.writer has no singleton row: this database was not initialized by \
             `gw admin init`"
                .to_owned(),
        )
    })?;
    Ok(row.try_get("epoch")?)
}
