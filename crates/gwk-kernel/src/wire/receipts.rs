//! Kernel-authored `pty_session` lifecycle receipts (P17).
//!
//! The kernel is the only observer of every PTY lifecycle moment WITH the
//! generation attached — the mirror mints the generation and the publish ack
//! never returns it — so all four receipt commands are authored here: open at
//! publish admission, close at typed retire and at the hangup sweep, attach
//! and detach at admission and stream end.
//!
//! A receipt failure NEVER fails the wire operation. The mux's availability
//! does not couple to ledger health: every error path here warns and returns.
//! The receipts feed the S8 cutover receipt's machine half; they are not a
//! precondition for serving a screen.
//!
//! Derivation: none — ledger plumbing only; no terminal byte or process
//! behavior is asserted here.

use std::sync::Arc;

use gwk_domain::command::KernelCommand;
use gwk_domain::envelope::{Actor, CommandEnvelope, ENVELOPE_SCHEMA_VERSION, Origin};
use gwk_domain::ids::{
    CommandId, IdempotencyKey, ProjectId, PtySessionGeneration, PtySessionId, RequestId, Timestamp,
};
use gwk_domain::protocol::{KernelErrorCode, KernelResult};

use super::pty::ConnectionId;
use crate::SYSTEM_PROJECT;
use crate::store::PgEventStore;

/// How many times a versioned receipt re-reads and re-submits after losing a
/// CAS race (attaches on one session can be concurrent), and how long it
/// waits for the open receipt to land when an attach outruns it.
const RETRIES: u32 = 3;
const RETRY_DELAY_MS: u64 = 25;

/// The ledger aggregate for one session LIFETIME: `{id}:{generation}`.
///
/// The mux reuses session ids — the estate's resident session is always
/// `console` — but the aggregate models one open and one terminal close per
/// row. Keying rows by id alone wedged the mirror on the first kernel restart
/// (observed live 2026-08-11): the surviving row made the next lifetime's
/// open a version-0 create against a version-2 aggregate, refused as
/// StaleVersion and silently dropped. Per-lifetime rows keep every promise
/// the receipt design makes — closed rows are history, peak concurrency
/// reads row open/close intervals, restarts count distinct generations.
fn lifetime(id: &PtySessionId, generation: &PtySessionGeneration) -> PtySessionId {
    gwk_domain::ids::pty_session_lifetime_id(id, generation)
}

/// Record that a publish opened a new session lifetime.
pub(crate) async fn opened(
    store: &Arc<PgEventStore>,
    id: &PtySessionId,
    generation: &PtySessionGeneration,
) {
    let key = format!("pty-open:{id}:{generation}");
    let command = KernelCommand::OpenPtySession {
        pty_session_id: lifetime(id, generation),
        generation: generation.clone(),
        engine_session_id: None,
        title: None,
    };
    // A stale answer here means a row for this exact lifetime already exists
    // — impossible unless the mirror double-opens. Loud either way: the id-
    // keyed wedge stayed invisible precisely because this return was dropped.
    if !submit(store, &key, &command).await {
        eprintln!(
            "gwk-kernel: pty receipt {key}: refused as stale — the lifetime row already exists"
        );
    }
}

/// Record that a session lifetime ended. `hangup` marks the sweep path — the
/// provenance the cutover receipt uses to tell a crash from a typed retire.
pub(crate) async fn closed(
    store: &Arc<PgEventStore>,
    id: &PtySessionId,
    generation: &PtySessionGeneration,
    hangup: bool,
) {
    let key = if hangup {
        format!("pty-close:{id}:{generation}:hangup")
    } else {
        format!("pty-close:{id}:{generation}")
    };
    let row = lifetime(id, generation);
    versioned(store, &row, &key, |expected_version| {
        KernelCommand::ClosePtySession {
            pty_session_id: row.clone(),
            expected_version,
        }
    })
    .await;
}

/// Record one admitted attach (styled or raw — both drive the counter).
///
/// The key embeds the CONNECTION as well as the request: request ids are
/// numbered per connection, so two concurrent clients both send `gw-1` and a
/// request-only key would refuse the second attach as an idempotency conflict
/// (observed live at the 2026-08-11 sitting). The connection id is unique for
/// the daemon's lifetime and the generation embeds the writer epoch, so the
/// triple never collides across restarts either.
pub(crate) async fn attached(
    store: &Arc<PgEventStore>,
    id: &PtySessionId,
    generation: &PtySessionGeneration,
    connection: ConnectionId,
    request_id: &RequestId,
) {
    let key = format!("pty-attach:{id}:{generation}:{connection}:{request_id}");
    let row = lifetime(id, generation);
    versioned(store, &row, &key, |expected_version| {
        KernelCommand::RecordPtyAttach {
            pty_session_id: row.clone(),
            expected_version,
        }
    })
    .await;
}

/// Everything the detach receipt needs, owned — the attach stream runs in a
/// task of its own, and the receipt outlives every borrow the admission held.
pub(crate) struct DetachReceipt {
    pub store: Arc<PgEventStore>,
    pub session_id: PtySessionId,
    pub generation: PtySessionGeneration,
    pub connection: ConnectionId,
    pub request_id: RequestId,
}

impl DetachReceipt {
    /// Record that the attach ended, whichever way it ended.
    pub(crate) async fn emit(self) {
        let key = format!(
            "pty-detach:{}:{}:{}:{}",
            self.session_id, self.generation, self.connection, self.request_id
        );
        let row = lifetime(&self.session_id, &self.generation);
        versioned(&self.store, &row, &key, |expected_version| {
            KernelCommand::RecordPtyDetach {
                pty_session_id: row.clone(),
                expected_version,
            }
        })
        .await;
    }
}

/// Carries a [`DetachReceipt`] through an attach task that can be ABORTED —
/// a closed connection drops its stream `JoinSet`, and a dropped future never
/// reaches its clean emit. Observed live at the 2026-08-11 sitting: client
/// hangup lost the detach silently. The clean path disarms the guard and
/// emits inline as before; a drop with the receipt still armed hands the emit
/// to a task of its own, so connection teardown cannot swallow it.
pub(crate) struct DetachOnDrop(Option<DetachReceipt>);

impl DetachOnDrop {
    pub(crate) fn new(receipt: DetachReceipt) -> Self {
        Self(Some(receipt))
    }

    /// The clean path: take the receipt to emit inline, disarming the drop arm.
    pub(crate) fn disarm(mut self) -> DetachReceipt {
        self.0
            .take()
            .expect("disarm consumes the guard exactly once")
    }
}

impl Drop for DetachOnDrop {
    fn drop(&mut self) {
        let Some(receipt) = self.0.take() else {
            return;
        };
        // Never blocks or fails the teardown path (the decoupling invariant):
        // outside a runtime the receipt is dropped loudly, matching every
        // other failure in this module.
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(receipt.emit());
            }
            Err(_) => eprintln!(
                "gwk-kernel: pty receipt pty-detach:{}:{}:{}:{}: dropped outside a runtime",
                receipt.session_id, receipt.generation, receipt.connection, receipt.request_id
            ),
        }
    }
}

/// Submit a version-bearing receipt: read the row's version, submit at it,
/// and retry a bounded number of times on a lost CAS race or on the row not
/// having landed yet (an attach can outrun its session's open receipt).
async fn versioned<F>(store: &Arc<PgEventStore>, id: &PtySessionId, key: &str, build: F)
where
    F: Fn(u32) -> KernelCommand,
{
    for _round in 0..RETRIES {
        let version: Option<i64> =
            match sqlx::query_scalar("SELECT version FROM gwk.pty_session WHERE id = $1")
                .bind(id.as_str())
                .fetch_optional(store.pool())
                .await
            {
                Ok(version) => version,
                Err(e) => {
                    eprintln!("gwk-kernel: pty receipt {key}: version read failed: {e}");
                    return;
                }
            };
        let Some(version) = version else {
            // The open receipt has not landed yet (or failed): wait once,
            // then give up loudly rather than block the wire task.
            tokio::time::sleep(std::time::Duration::from_millis(RETRY_DELAY_MS)).await;
            continue;
        };
        let Ok(version) = u32::try_from(version) else {
            eprintln!("gwk-kernel: pty receipt {key}: version {version} out of u32 range");
            return;
        };
        if submit(store, key, &build(version)).await {
            return;
        }
    }
    eprintln!("gwk-kernel: pty receipt {key}: dropped after {RETRIES} rounds");
}

/// Submit one kernel-authored receipt envelope. `true` = settled (applied or
/// permanently refused — both end the caller's retry loop); `false` = lost a
/// CAS race and worth re-reading.
async fn submit(store: &Arc<PgEventStore>, key: &str, command: &KernelCommand) -> bool {
    let Some(issued_at) = db_now(store).await else {
        eprintln!("gwk-kernel: pty receipt {key}: no clock, dropped");
        return true;
    };
    let envelope = CommandEnvelope {
        command_id: CommandId::new(format!("cmd-{key}")),
        project_id: ProjectId::new(SYSTEM_PROJECT),
        command_type: command.command_type().to_owned(),
        schema_version: ENVELOPE_SCHEMA_VERSION,
        issued_at,
        actor: Actor {
            kind: "kernel".to_owned(),
            id: None,
        },
        origin: Origin {
            system: "kernel".into(),
            r#ref: None,
        },
        target_aggregate_type: None,
        target_aggregate_id: None,
        expected_version: None,
        idempotency_key: IdempotencyKey::new(key),
        causation_id: None,
        correlation_id: None,
        payload: match serde_json::to_value(command) {
            Ok(payload) => payload,
            Err(e) => {
                eprintln!("gwk-kernel: pty receipt {key}: serialize failed: {e}");
                return true;
            }
        },
    };
    match store.submit(&envelope).await {
        KernelResult::CommandApplied { .. } => true,
        KernelResult::Error { code, message, .. } => {
            if code == KernelErrorCode::StaleVersion {
                return false;
            }
            eprintln!("gwk-kernel: pty receipt {key}: refused: {code:?} {message}");
            true
        }
        other => {
            eprintln!("gwk-kernel: pty receipt {key}: unexpected answer: {other:?}");
            true
        }
    }
}

/// The estate's one time authority is postgres: receipts stamp `issued_at`
/// from the database clock, the same clock `appended_at` already uses.
async fn db_now(store: &Arc<PgEventStore>) -> Option<Timestamp> {
    match sqlx::query_scalar::<_, String>("SELECT to_json(now()) #>> '{}'")
        .fetch_one(store.pool())
        .await
    {
        Ok(now) => Some(Timestamp::new(now)),
        Err(e) => {
            eprintln!("gwk-kernel: pty receipt: db clock read failed: {e}");
            None
        }
    }
}
