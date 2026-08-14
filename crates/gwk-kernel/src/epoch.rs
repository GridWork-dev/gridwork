//! The epoch: one genesis append, the seal it opens under, and the activation
//! that breaks it.
//!
//! The kernel's own lifecycle is an aggregate like every other one —
//! `kernel`/`singleton`, in project `system` — which is why no state table
//! stands behind it. Version 1 is genesis and the kernel is SEALED; version 2
//! is activation and it is ACTIVE. So `max(aggregate_version)` on that
//! aggregate IS the epoch, read under the same writer lock the append will
//! take. A cached flag or a projection row would be a second place the answer
//! lives, and the log is the one that survives a restore.
//!
//! Genesis is appended through the fenced store, never beside it:
//! [`crate::admin::init`] runs as the admin role and holds neither the writer
//! lock nor the durable epoch, so a genesis written there would be the one
//! event in the log that skipped the path every other event takes.

use gwk_domain::envelope::{Actor, ENVELOPE_SCHEMA_VERSION, EventEnvelope, Origin};
use gwk_domain::ids::{
    AggregateId, EventId, FenceToken, IdempotencyKey, ProjectId, Seq, Timestamp,
};
use gwk_domain::protocol::{CONTRACT_VERSION, KernelErrorCode};
use sqlx::PgConnection;

use crate::project::Refusal;
use crate::store::{PgEventStore, current_aggregate_version};

/// The aggregate the kernel keeps its own lifecycle in.
pub const KERNEL_AGGREGATE: &str = "kernel";
/// Its only id: there is one kernel per database.
pub const KERNEL_SINGLETON: &str = "singleton";
/// The project genesis is written under, and therefore — by the ownership rule
/// in [`crate::submit`] — the only project that may activate.
pub const SYSTEM_PROJECT: &str = "system";
/// Genesis carries a fixed key: there is no caller to supply one, and a fixed
/// key makes a second genesis attempt a constraint violation rather than a
/// second event.
pub const GENESIS_KEY: &str = "kernel_initialized:v1";
pub const GENESIS_EVENT_TYPE: &str = "kernel_initialized";
pub const ACTIVATION_EVENT_TYPE: &str = "kernel_activated";

/// Length of a full git revision — the form the genesis payload records.
const REVISION_HEX_LEN: usize = 40;

const COMMITTED_CUTOVER: &str = "SELECT payload ->> 'cutover_id' FROM gwk.event \
     WHERE aggregate_type = 'kernel' AND aggregate_id = 'singleton' \
       AND event_type = 'kernel_activated' \
     ORDER BY aggregate_version LIMIT 1";
// The database's clock, not the process's: `appended_at` is assigned by `now()`
// inside the append, so taking `occurred_at` from anywhere else would put two
// clocks in one row.
const DB_NOW: &str = "SELECT to_json(now()) #>> '{}'";

/// Where this kernel is in its own lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Epoch {
    /// No genesis event. Nothing may be submitted — including activation,
    /// which asserts version 1 and would find 0.
    None,
    /// Genesis only. The sealed allowlist is in force.
    Sealed,
    /// Activated. Business commands are admitted.
    Active,
}

impl Epoch {
    fn of_version(version: u32) -> Self {
        match version {
            0 => Self::None,
            1 => Self::Sealed,
            _ => Self::Active,
        }
    }
}

/// Read the epoch. Call it under the writer lock, so the answer and the append
/// that depends on it are the same instant.
pub(crate) async fn epoch_of(conn: &mut PgConnection) -> Result<Epoch, Refusal> {
    let version = current_aggregate_version(conn, KERNEL_AGGREGATE, KERNEL_SINGLETON).await?;
    Ok(Epoch::of_version(version))
}

/// The database's clock, RFC 3339, as the log itself stamps it.
///
/// Exposed because a projection page's `served_at` has to come from the same
/// clock as the `appended_at` and `recorded_at` on the rows it describes. The
/// `DB_NOW` comment above already states the rule for events — two clocks in
/// one row is the failure — and an answer that pairs kernel rows with a
/// process-clock timestamp is the same failure spread across two fields
/// instead of two rows. A client comparing "today" against `recorded_at` would
/// be comparing against a boundary the row's own clock never agreed to.
pub(crate) async fn db_now(conn: &mut PgConnection) -> Result<String, Refusal> {
    sqlx::query_scalar(DB_NOW)
        .fetch_one(conn)
        .await
        .map_err(|e| Refusal::storage(format!("read the database clock: {e}")))
}

/// The cutover this kernel actually activated at, if it has.
pub(crate) async fn committed_cutover(conn: &mut PgConnection) -> Result<Option<String>, Refusal> {
    sqlx::query_scalar(COMMITTED_CUTOVER)
        .fetch_optional(conn)
        .await
        .map(Option::flatten)
        .map_err(|e| Refusal::storage(format!("read the committed cutover: {e}")))
}

/// The one idempotency key an activation may carry.
///
/// Required of the caller rather than derived from the body, because the two
/// disagree in a way that matters: deriving it would let two envelopes with
/// different keys collapse onto one event key, and the second would come back
/// as a storage-level collision instead of the typed answer the caller needs.
pub(crate) fn activation_key(cutover_id: &str) -> String {
    format!("{ACTIVATION_EVENT_TYPE}:{cutover_id}")
}

/// True iff `value` is a full lowercase 40-hex git revision.
///
/// Abbreviated revisions are refused: the genesis payload is immutable and is
/// what a later build compares itself against, so a prefix that is unique today
/// and ambiguous in a year would make that comparison unanswerable.
pub fn is_public_revision(value: &str) -> bool {
    value.len() == REVISION_HEX_LEN
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl PgEventStore {
    /// Append genesis if this log has none. Idempotent: a kernel that already
    /// has an epoch is left exactly as it was.
    ///
    /// Separate from [`PgEventStore::open`] on purpose — opening a store reads,
    /// and a constructor that writes would put an append in the path of every
    /// replay and every read-only inspection of a log.
    pub async fn ensure_genesis(&self, public_revision: &str) -> Result<(), Refusal> {
        if !is_public_revision(public_revision) {
            return Err(Refusal::validation(format!(
                "genesis records the full 40-character public revision, not {public_revision:?}"
            )));
        }
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| Refusal::storage(format!("begin: {e}")))?;
        let writer = self.lock_writer(&mut tx).await?;
        // Under the lock, so the check and the append cannot be separated by
        // another writer's genesis.
        if epoch_of(&mut tx).await? != Epoch::None {
            return Ok(());
        }
        let now: String = sqlx::query_scalar(DB_NOW)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| Refusal::storage(format!("read the database clock: {e}")))?;
        let fence = writer.current_fence.map(FenceToken::new);
        self.append_locked(
            &mut tx,
            &writer,
            0,
            fence,
            &[genesis_event(public_revision, &now)],
        )
        .await?;
        // No projection: the epoch boundary is the log itself. That is also why
        // the genesis payload is not a `KernelCommand` — nothing issued it.
        tx.commit()
            .await
            .map_err(|e| Refusal::storage(format!("commit: {e}")))?;
        Ok(())
    }
}

/// The genesis event, at the version and under the key the contract pins.
///
/// `event_id` is derived exactly as [`crate::submit`] derives every other one,
/// so genesis is not a special case to anything that reads ids.
fn genesis_event(public_revision: &str, at: &str) -> EventEnvelope {
    EventEnvelope {
        event_id: EventId::new(format!("{KERNEL_AGGREGATE}:{KERNEL_SINGLETON}:1")),
        project_id: ProjectId::new(SYSTEM_PROJECT),
        aggregate_type: KERNEL_AGGREGATE.to_owned(),
        aggregate_id: AggregateId::new(KERNEL_SINGLETON),
        aggregate_version: 1,
        event_type: GENESIS_EVENT_TYPE.to_owned(),
        schema_version: ENVELOPE_SCHEMA_VERSION,
        // Assigned inside the append, like every other event's.
        global_sequence: Seq::new(0),
        appended_at: Timestamp::new(at),
        occurred_at: Timestamp::new(at),
        actor: Actor {
            kind: "kernel".to_owned(),
            id: None,
        },
        origin: Origin {
            system: "gw".to_owned(),
            r#ref: None,
        },
        causation_id: None,
        correlation_id: None,
        idempotency_key: Some(IdempotencyKey::new(GENESIS_KEY)),
        // Exactly what the contract says genesis records, and nothing more: the
        // number a client compares its own contract against, and the revision
        // this binary was built from.
        payload: serde_json::json!({
            "contract_version": CONTRACT_VERSION,
            "public_revision": public_revision,
        }),
        payload_ref: None,
    }
}

/// The refusal a sealed — or epoch-less — kernel answers with.
///
/// [`KernelErrorCode::Sealed`] covers both: a kernel with no genesis is a
/// kernel whose sealed allowlist is empty, and giving it a different code would
/// mean adding one to a frozen set to describe a state the client reacts to
/// identically — it cannot proceed, and only an operator can change that.
pub(crate) fn sealed_refusal(epoch: Epoch, command_type: &str) -> Refusal {
    let message = match epoch {
        Epoch::None => {
            "this kernel has no epoch: genesis has not been appended, so nothing may be \
             submitted — not even activation, which asserts the sealed version"
                .to_owned()
        }
        _ => format!("this kernel is sealed; {command_type} is not on the sealed allowlist"),
    };
    Refusal::new(KernelErrorCode::Sealed, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_version_is_the_epoch() {
        assert_eq!(Epoch::of_version(0), Epoch::None);
        assert_eq!(Epoch::of_version(1), Epoch::Sealed);
        assert_eq!(Epoch::of_version(2), Epoch::Active);
        // There is no version above 2 today, but the log is append-only and a
        // later kernel event must not read as "sealed again".
        assert_eq!(Epoch::of_version(9), Epoch::Active);
    }

    #[test]
    fn only_a_full_lowercase_revision_is_one() {
        assert!(is_public_revision(&"a1b2c3d4e5".repeat(4)));
        assert!(!is_public_revision(&"A1B2C3D4E5".repeat(4)));
        assert!(!is_public_revision("a1b2c3d4"));
        assert!(!is_public_revision(""));
        assert!(!is_public_revision(&"z".repeat(40)));
    }

    #[test]
    fn the_activation_key_names_its_cutover() {
        assert_eq!(activation_key("c-1"), "kernel_activated:c-1");
    }
}
