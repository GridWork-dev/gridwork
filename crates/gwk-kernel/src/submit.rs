//! The command path: one [`CommandEnvelope`] in, one committed event plus its
//! projection out.
//!
//! The order of the steps is the design, not an implementation detail:
//!
//! 1. **Lock the writer row** — before ANY read this decision depends on. The
//!    lock is held to commit, so reading the current version after taking it is
//!    what makes the decision and the write the same instant.
//! 2. **Answer a replay from the log** — project-wide, by key. Doing this ahead
//!    of the CAS is what makes a retry stable: a retry presents the same
//!    `expected_version` it did the first time, which the CAS would now call a
//!    conflict.
//! 3. **Decide** — [`gwk_domain::transition::apply`] for the two state machines
//!    in this phase, a plain version compare for the rest.
//! 4. **Append and project in ONE transaction.** The plan requires events and
//!    projections to land together; splitting them would let a crash leave a
//!    log the projections do not reflect.
//!
//! Before any of it sits the **epoch** ([`crate::epoch`]): a log with only its
//! genesis event is SEALED and admits `activate_kernel` alone, and a log
//! without even that admits nothing. The check is read under the writer lock
//! taken in step 1, and activation's own CAS re-proves it at append time.
//!
//! Between ownership and the decision sits **authority**: a gated command is
//! evaluated against the grants on record, and a page commits its receipt and
//! attention item before refusing. That is the one refusal here that leaves
//! rows behind, and deliberately so — a page whose evidence rolls back cannot
//! be told apart from a command nobody sent.
//!
//! Scope is everything except ingestion: task, attempt, engine session, lease,
//! worktree, dispatch node, budget, checkpoint, round, finding, message,
//! execution command, gate, evidence, authority grant, and attention item.
//! `IngestRecord` is refused by name here until task 17 lands it — an
//! unrecognized command is never a no-op.

use gwk_domain::command::KernelCommand;
use gwk_domain::envelope::{
    CommandEnvelope, ENVELOPE_SCHEMA_VERSION, EventEnvelope, accept_schema_version,
};
use gwk_domain::fsm::{AttemptState, CommandState, MessageState, TaskState};
use gwk_domain::ids::{AggregateId, EventId, ReceiptId, Seq};
use gwk_domain::port::EventStore;
use gwk_domain::protocol::{KernelErrorCode, KernelResult};
use gwk_domain::transition::{self, Cursor, TransitionRequest, TransitionResult};
use serde::de::DeserializeOwned;
use sqlx::{PgConnection, Row};

use crate::authority;
use crate::epoch::{self, Epoch};
use crate::numeric::from_numeric_text;
use crate::project::{
    Refusal, apply_event, from_wire_str, page_attention, unresolved_attention, wire_str,
    write_receipt,
};
use crate::store::{MAX_INFLIGHT_APPENDS, PgEventStore, current_aggregate_version, events_for_key};

// One literal per read: sqlx 0.9 refuses a runtime-built query string, and a
// table name cannot be a bind parameter — so the set of tables this path may
// read is fixed here, in the open, rather than assembled from a caller's input.
const TASK_CURSOR: &str = "SELECT state, version FROM gwk.task WHERE id = $1";
const ATTEMPT_CURSOR: &str = "SELECT state, version FROM gwk.attempt WHERE id = $1";
const ATTEMPT_VERSION: &str = "SELECT version FROM gwk.attempt WHERE id = $1";
const LEASE_VERSION: &str = "SELECT version FROM gwk.lease WHERE id = $1";
const DISPATCH_NODE_VERSION: &str = "SELECT version FROM gwk.dispatch_node WHERE id = $1";
const AGGREGATE_OWNER: &str = "SELECT project_id FROM gwk.event \
     WHERE aggregate_type = $1 AND aggregate_id = $2 \
     ORDER BY aggregate_version LIMIT 1";
const MESSAGE_CURSOR: &str = "SELECT state, version FROM gwk.message WHERE id = $1";
const COMMAND_CURSOR: &str = "SELECT state, version FROM gwk.command WHERE id = $1";
const GATE_VERSION: &str = "SELECT version FROM gwk.gate WHERE id = $1";
const CHECKPOINT_SEQ: &str =
    "SELECT seq::text AS seq_text FROM gwk.orchestrator_checkpoint WHERE orchestrator_id = $1";

/// Where a command's event belongs and what it is called there.
#[derive(Debug)]
struct Route {
    aggregate_type: &'static str,
    aggregate_id: String,
    event_type: &'static str,
}

/// What the log already holds under this command's idempotency key.
enum Prior {
    /// Nothing: this key has never been used in this project.
    Unused,
    /// The identical request, already applied. Its original events ARE the
    /// answer — re-deciding would double-apply.
    Replay(Vec<EventEnvelope>),
    /// The key is taken by a different request. Refused rather than landed
    /// beside it, which is the whole point of requiring a key.
    Conflict(String),
}

impl PgEventStore {
    /// Apply one command, or answer why not.
    ///
    /// A refusal is a value here, exactly as the contract says: this returns
    /// [`KernelResult::Error`] rather than an `Err`, so the wire layer has
    /// nothing left to translate.
    pub async fn submit(&self, envelope: &CommandEnvelope) -> KernelResult {
        match self.try_submit(envelope).await {
            Ok(result) => result,
            Err(refusal) => refusal.into_result(),
        }
    }

    async fn try_submit(&self, envelope: &CommandEnvelope) -> Result<KernelResult, Refusal> {
        let _permit = self.admit().map_err(|_| {
            Refusal::new(
                KernelErrorCode::Overloaded,
                format!("append queue is full ({MAX_INFLIGHT_APPENDS} in flight)"),
            )
        })?;
        accept_schema_version(envelope.schema_version, ENVELOPE_SCHEMA_VERSION, &[])
            .map_err(|e| Refusal::new(KernelErrorCode::Schema, e.to_string()))?;
        let command = KernelCommand::from_envelope(envelope)
            .map_err(|e| Refusal::validation(e.to_string()))?;
        let route = route_of(&command)?;
        check_routing(envelope, &route)?;
        check_body_project(envelope, &command)?;
        check_activation(envelope, &command)?;
        let payload = serde_json::to_value(&command)
            .map_err(|e| Refusal::storage(format!("serialize command body: {e}")))?;

        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| Refusal::storage(format!("begin: {e}")))?;
        // FIRST, and before every read below: everything this transaction
        // decides is read under this lock, so no concurrent writer can move the
        // ground between the decision and the append.
        let writer = self.lock_writer(&mut tx).await?;
        // Under the lock and before the key check: a sealed kernel refuses
        // whether or not this key has been seen, and answering "replay" out of
        // a log the caller may not read from yet would be a leak dressed as a
        // convenience.
        let epoch = epoch::epoch_of(&mut tx).await?;
        if !admitted(epoch, &command) {
            return Err(epoch::sealed_refusal(epoch, command.command_type()));
        }

        let expected_version = match prior_for_key(&mut tx, envelope, &route, &payload).await? {
            Prior::Replay(events) => {
                // Nothing to write; the commit only releases the lock. The
                // projections this key produced were written by the original
                // append and re-applying them would advance a CAS version no
                // event accounts for.
                tx.commit()
                    .await
                    .map_err(|e| Refusal::storage(format!("commit: {e}")))?;
                return self.applied(envelope, events).await;
            }
            Prior::Conflict(reason) => {
                return Err(Refusal::new(KernelErrorCode::IdempotencyConflict, reason));
            }
            Prior::Unused => {
                // After the key check, not before it: a reused key is a
                // conflict whoever sends it, and reporting the key is the
                // answer a retrier can act on. Ownership is the question that
                // only arises once the request is genuinely new.
                check_aggregate_owner(&mut tx, envelope, &route).await?;
                check_second_cutover(&mut tx, &command, epoch).await?;
                let decision = authority::evaluate(
                    &mut tx,
                    envelope,
                    &envelope.command_type,
                    &route.aggregate_id,
                )
                .await?;
                if let authority::Decision::Page { action_class } = decision {
                    // A page does NOT mutate the target, so this returns before
                    // `decide` — but it still commits, because the receipt and
                    // the attention item are the whole point of paging. This is
                    // the one refusal in the kernel that leaves rows behind.
                    return self.paged(tx, envelope, &route, action_class).await;
                }
                if let Some(action_class) = decision.action_class() {
                    write_receipt(
                        &mut tx,
                        &receipt_for(
                            envelope,
                            &route,
                            action_class,
                            "matching unexpired scoped grant",
                        ),
                    )
                    .await?;
                }
                check_attention_dedup(&mut tx, &command).await?;
                decide(&mut tx, envelope, &command, &route).await?
            }
        };
        check_expected_version(envelope, expected_version)?;

        let event = build_event(envelope, payload, &route, expected_version)?;
        // The kernel's own command path is authorized by the writer LOCK plus
        // the durable EPOCH `lock_writer` just compared — both strictly stronger
        // than a fence token, which exists for an EXTERNAL append actor holding
        // neither. Presenting the current token satisfies the port's check
        // without pretending this path arrived fenced.
        let fence = writer.current_fence.map(gwk_domain::ids::FenceToken::new);
        let appended = self
            .append_locked(&mut tx, &writer, expected_version, fence, &[event])
            .await?;
        if !appended.replayed {
            for event in &appended.events {
                apply_event(&mut tx, event).await?;
            }
        }
        tx.commit()
            .await
            .map_err(|e| Refusal::storage(format!("commit: {e}")))?;

        self.applied(envelope, appended.events).await
    }

    /// The paged answer: a refusal that commits its own trail.
    ///
    /// The receipt and the attention item are written and COMMITTED even though
    /// the command is refused, because a page whose evidence rolls back is
    /// indistinguishable from a command that was never sent — and the operator
    /// finding out is the entire mechanism.
    async fn paged(
        &self,
        mut tx: sqlx::Transaction<'_, sqlx::Postgres>,
        envelope: &CommandEnvelope,
        route: &Route,
        action_class: &'static str,
    ) -> Result<KernelResult, Refusal> {
        let actor = actor_json(envelope)?;
        let at = envelope.issued_at.as_str();
        let subject = format!("{}/{}", route.aggregate_type, route.aggregate_id);
        write_receipt(
            &mut tx,
            &receipt_for(
                envelope,
                route,
                action_class,
                "no matching unexpired scoped grant",
            ),
        )
        .await?;
        page_attention(
            &mut tx,
            // Derived from what the dedup key already is, so a retry lands on
            // the same id the first page created rather than minting a rival
            // the index then refuses.
            &format!("page:{action_class}:{subject}"),
            &format!(
                "{} requires an unexpired {action_class} grant",
                envelope.command_type
            ),
            &subject,
            &actor,
            at,
        )
        .await?;
        tx.commit()
            .await
            .map_err(|e| Refusal::storage(format!("commit: {e}")))?;
        Err(Refusal::new(
            KernelErrorCode::Authority,
            format!(
                "{} on {subject} requires an unexpired {action_class} grant; attention raised",
                envelope.command_type
            ),
        )
        .with_detail(serde_json::json!({ "action_class": action_class, "subject": subject })))
    }

    /// The success answer, with the watermark read after the commit that
    /// produced it.
    async fn applied(
        &self,
        envelope: &CommandEnvelope,
        events: Vec<EventEnvelope>,
    ) -> Result<KernelResult, Refusal> {
        let watermark = self.watermark().await?.ok_or_else(|| {
            Refusal::storage("the log is empty after a committed append".to_owned())
        })?;
        Ok(KernelResult::CommandApplied {
            command_id: envelope.command_id.clone(),
            events,
            watermark,
        })
    }
}

/// Which aggregate a command addresses, and the event it produces there.
///
/// One match rather than two: an aggregate without an event name (or the
/// reverse) would be a routing bug that only shows up in the log.
fn route_of(command: &KernelCommand) -> Result<Route, Refusal> {
    use KernelCommand as C;
    let (aggregate_type, aggregate_id, event_type) = match command {
        // The kernel's own lifecycle is an aggregate like any other, which is
        // what lets the epoch be read off the log instead of a state table.
        C::ActivateKernel { .. } => (
            epoch::KERNEL_AGGREGATE,
            epoch::KERNEL_SINGLETON.to_owned(),
            epoch::ACTIVATION_EVENT_TYPE,
        ),

        C::CreateTask { task_id, .. } => ("task", task_id.as_str().to_owned(), "task_created"),
        C::TransitionTask { task_id, .. } => {
            ("task", task_id.as_str().to_owned(), "task_transitioned")
        }

        C::CreateAttempt { attempt_id, .. } => {
            ("attempt", attempt_id.as_str().to_owned(), "attempt_created")
        }
        C::TransitionAttempt { attempt_id, .. } => (
            "attempt",
            attempt_id.as_str().to_owned(),
            "attempt_transitioned",
        ),
        C::RecordAttemptOutcome { attempt_id, .. } => (
            "attempt",
            attempt_id.as_str().to_owned(),
            "attempt_outcome_recorded",
        ),
        C::UpdateBudget { attempt_id, .. } => {
            ("attempt", attempt_id.as_str().to_owned(), "budget_updated")
        }
        C::RecordRound { attempt_id, .. } => {
            ("attempt", attempt_id.as_str().to_owned(), "round_recorded")
        }
        C::RecordFinding { attempt_id, .. } => (
            "attempt",
            attempt_id.as_str().to_owned(),
            "finding_recorded",
        ),

        C::OpenEngineSession {
            engine_session_id, ..
        } => (
            "engine_session",
            engine_session_id.as_str().to_owned(),
            "engine_session_opened",
        ),
        C::CloseEngineSession { engine_session_id } => (
            "engine_session",
            engine_session_id.as_str().to_owned(),
            "engine_session_closed",
        ),

        C::AcquireLease { lease_id, .. } => {
            ("lease", lease_id.as_str().to_owned(), "lease_acquired")
        }
        C::RenewLease { lease_id, .. } => ("lease", lease_id.as_str().to_owned(), "lease_renewed"),
        C::ReleaseLease { lease_id, .. } => {
            ("lease", lease_id.as_str().to_owned(), "lease_released")
        }
        C::ExpireLease { lease_id, .. } => ("lease", lease_id.as_str().to_owned(), "lease_expired"),

        C::RegisterWorktree { worktree_id, .. } => (
            "worktree",
            worktree_id.as_str().to_owned(),
            "worktree_registered",
        ),
        C::UpdateWorktree { worktree_id, .. } => (
            "worktree",
            worktree_id.as_str().to_owned(),
            "worktree_updated",
        ),
        C::ReleaseWorktree { worktree_id, .. } => (
            "worktree",
            worktree_id.as_str().to_owned(),
            "worktree_released",
        ),

        C::RegisterDispatchNode {
            dispatch_node_id, ..
        } => (
            "dispatch_node",
            dispatch_node_id.as_str().to_owned(),
            "dispatch_node_registered",
        ),
        C::TransitionDispatchNode {
            dispatch_node_id, ..
        } => (
            "dispatch_node",
            dispatch_node_id.as_str().to_owned(),
            "dispatch_node_transitioned",
        ),

        C::SendMessage { message_id, .. } => {
            ("message", message_id.as_str().to_owned(), "message_sent")
        }
        C::TransitionMessage { message_id, .. } => (
            "message",
            message_id.as_str().to_owned(),
            "message_transitioned",
        ),

        C::IssueCommand { command_id, .. } => {
            ("command", command_id.as_str().to_owned(), "command_issued")
        }
        C::TransitionCommand { command_id, .. } => (
            "command",
            command_id.as_str().to_owned(),
            "command_transitioned",
        ),
        C::RecordCommandOutcome { command_id, .. } => (
            "command",
            command_id.as_str().to_owned(),
            "command_outcome_recorded",
        ),

        C::OpenGate { gate_id, .. } => ("gate", gate_id.as_str().to_owned(), "gate_opened"),
        C::DecideGate { gate_id, .. } => ("gate", gate_id.as_str().to_owned(), "gate_decided"),

        C::RecordEvidence { evidence_id, .. } => (
            "evidence",
            evidence_id.as_str().to_owned(),
            "evidence_recorded",
        ),

        C::GrantAuthority {
            authority_grant_id, ..
        } => (
            "authority_grant",
            authority_grant_id.as_str().to_owned(),
            "authority_granted",
        ),
        C::RevokeAuthority {
            authority_grant_id, ..
        } => (
            "authority_grant",
            authority_grant_id.as_str().to_owned(),
            "authority_revoked",
        ),

        C::RaiseAttention {
            attention_item_id, ..
        } => (
            "attention_item",
            attention_item_id.as_str().to_owned(),
            "attention_raised",
        ),
        C::ResolveAttention {
            attention_item_id, ..
        } => (
            "attention_item",
            attention_item_id.as_str().to_owned(),
            "attention_resolved",
        ),

        C::WriteOrchestratorCheckpoint { checkpoint } => (
            "orchestrator_checkpoint",
            // The store is keyed on it, so an unnamed checkpoint has no row to
            // be the latest of. Refused at the boundary rather than defaulted
            // to some placeholder id every anonymous orchestrator would share.
            checkpoint.orchestrator_id.clone().ok_or_else(|| {
                Refusal::validation("a checkpoint without an orchestrator_id has no identity")
            })?,
            "orchestrator_checkpoint_written",
        ),

        other => {
            return Err(Refusal::validation(format!(
                "{} is not accepted by this kernel yet",
                other.command_type()
            )));
        }
    };
    if aggregate_id.is_empty() {
        return Err(Refusal::validation(format!(
            "{} names an empty {aggregate_type} id",
            command.command_type()
        )));
    }
    Ok(Route {
        aggregate_type,
        aggregate_id,
        event_type,
    })
}

/// The envelope's routing metadata must name the aggregate its body does.
///
/// `KernelCommand::from_envelope` already refuses an envelope whose
/// `command_type` disagrees with its body, for the reason that a handler chosen
/// by the metadata would otherwise run a body of some other shape. These two
/// fields are the same metadata pointing at the same body; leaving them
/// unchecked would mean a caller can label an envelope for one aggregate and
/// mutate another, and be told it succeeded.
fn check_routing(envelope: &CommandEnvelope, route: &Route) -> Result<(), Refusal> {
    let mismatch = |field: &str, declared: &str| {
        Refusal::validation(format!(
            "envelope {field} {declared:?} does not name the aggregate the body addresses \
             ({}/{})",
            route.aggregate_type, route.aggregate_id
        ))
    };
    if let Some(declared) = &envelope.target_aggregate_type
        && declared != route.aggregate_type
    {
        return Err(mismatch("target_aggregate_type", declared));
    }
    if let Some(declared) = &envelope.target_aggregate_id
        && declared.as_str() != route.aggregate_id
    {
        return Err(mismatch("target_aggregate_id", declared.as_str()));
    }
    Ok(())
}

/// The receipt id for one command's authority result.
///
/// `(project_id, idempotency_key)` is already globally unique by
/// `event_idempotency_project`, which makes this collision-free and stable
/// across a retry — a denied or paged command writes no event, so the retry
/// has to derive the same id or the ledger grows a duplicate per attempt.
//
// ponytail: `:`-joined like `event_id` elsewhere in this path. Two keys that
// differ only in where a `:` falls would collide; the house already accepts
// that for event ids, and the alternative is a length-prefixed encoding no
// human can read in a psql session.
fn receipt_for(
    envelope: &CommandEnvelope,
    route: &Route,
    action_class: &str,
    observed_basis: &str,
) -> gwk_domain::entity::Receipt {
    gwk_domain::entity::Receipt {
        id: ReceiptId::new(format!(
            "receipt:{}:{}",
            envelope.project_id.as_str(),
            envelope.idempotency_key.as_str()
        )),
        actor: envelope.actor.clone(),
        action: action_class.to_owned(),
        subject_type: route.aggregate_type.to_owned(),
        subject_id: route.aggregate_id.clone(),
        // No edge: an authority result attests a DECISION about a command, not
        // a state flip. The flip receipt the liveness rule needs is a different
        // row with `from`/`to` set, and conflating them would make the ledger
        // unable to answer which kind of fact it is holding.
        from: None,
        to: None,
        observed_basis: Some(observed_basis.to_owned()),
        // The command's own time, not the clock: replay must rebuild the same
        // ledger it built live.
        ts: envelope.issued_at.clone(),
    }
}

fn actor_json(envelope: &CommandEnvelope) -> Result<serde_json::Value, Refusal> {
    serde_json::to_value(&envelope.actor)
        .map_err(|e| Refusal::storage(format!("serialize actor: {e}")))
}

/// An explicit `RaiseAttention` may not open a second item over an open one.
///
/// The kernel's own page path silently joins the existing item, because it has
/// no id of its own to honor. A caller-issued command does: it named an id and
/// expects a row under it, so a duplicate is refused by name rather than
/// accepted as a write that quietly did nothing.
async fn check_attention_dedup(
    conn: &mut PgConnection,
    command: &KernelCommand,
) -> Result<(), Refusal> {
    let KernelCommand::RaiseAttention {
        kind, subject_ref, ..
    } = command
    else {
        return Ok(());
    };
    if let Some(open) = unresolved_attention(conn, kind, subject_ref.as_deref()).await? {
        return Err(Refusal::validation(format!(
            "attention item {open:?} is already open on ({kind:?}, {:?})",
            subject_ref.as_deref().unwrap_or_default()
        )));
    }
    Ok(())
}

/// A create that names its own project must name the one the envelope routes to.
///
/// `CreateTask` carries a `project` field and the envelope carries
/// `project_id`; both end up describing the same task, so two different values
/// is a caller mistake with no correct reading — the row would say one thing
/// and its log another. Same treatment as the target fields above: refuse
/// rather than silently pick a winner. An absent body field stays absent.
fn check_body_project(envelope: &CommandEnvelope, command: &KernelCommand) -> Result<(), Refusal> {
    let KernelCommand::CreateTask {
        project: Some(project),
        ..
    } = command
    else {
        return Ok(());
    };
    if project != envelope.project_id.as_str() {
        return Err(Refusal::validation(format!(
            "body project {project:?} does not name the envelope's project {:?}",
            envelope.project_id.as_str()
        )));
    }
    Ok(())
}

/// The sealed allowlist, as a pattern rather than a name.
///
/// Matching the variant instead of comparing `command_type` means adding a
/// command cannot quietly widen what a sealed kernel accepts: the allowlist is
/// one arm here, and everything else is the fallthrough.
fn admitted(epoch: Epoch, command: &KernelCommand) -> bool {
    match epoch {
        Epoch::Active => true,
        Epoch::Sealed => matches!(command, KernelCommand::ActivateKernel { .. }),
        Epoch::None => false,
    }
}

/// What an activation must carry before it is allowed to touch the log.
///
/// The cutover boundary is the one irreversible write in this kernel, so both
/// of its payload fields are checked here rather than left to be discovered
/// later: `from_envelope` validates shape, not content, and a malformed digest
/// in an immutable event is unfixable. The key is REQUIRED to equal
/// [`epoch::activation_key`] rather than being derived from the body — see that
/// function for why.
fn check_activation(envelope: &CommandEnvelope, command: &KernelCommand) -> Result<(), Refusal> {
    let KernelCommand::ActivateKernel {
        cutover_id,
        archive_manifest_sha256,
    } = command
    else {
        return Ok(());
    };
    if cutover_id.is_empty() {
        return Err(Refusal::validation(
            "an activation with an empty cutover id names no cutover",
        ));
    }
    if !gwk_domain::blob::is_sha256_hex(archive_manifest_sha256) {
        return Err(Refusal::validation(format!(
            "archive_manifest_sha256 must be a lowercase 64-hex digest, not \
             {archive_manifest_sha256:?}"
        )));
    }
    let required = epoch::activation_key(cutover_id);
    if envelope.idempotency_key.as_str() != required {
        return Err(Refusal::validation(format!(
            "activating cutover {cutover_id:?} requires idempotency key {required:?}, not {:?}",
            envelope.idempotency_key.as_str()
        )));
    }
    Ok(())
}

/// An activated kernel has one cutover, and it is not this one.
///
/// Reached only with an unused key, which for an activation already means a
/// different cutover id — the same one would have come back as a replay, and
/// the same id with a different manifest as an idempotency conflict. So the
/// answer names the cutover that actually took: the alternative is a bare CAS
/// refusal ("expected 1, found 2") at the moment an operator most needs to know
/// which epoch they are standing in.
async fn check_second_cutover(
    conn: &mut PgConnection,
    command: &KernelCommand,
    epoch: Epoch,
) -> Result<(), Refusal> {
    let KernelCommand::ActivateKernel { cutover_id, .. } = command else {
        return Ok(());
    };
    if epoch != Epoch::Active {
        return Ok(());
    }
    let committed = epoch::committed_cutover(conn).await?.ok_or_else(|| {
        Refusal::storage("the kernel is past genesis with no activation event".to_owned())
    })?;
    Err(Refusal::new(
        KernelErrorCode::AlreadyActive,
        format!(
            "this kernel activated at cutover {committed:?}; {cutover_id:?} is a different \
                 cutover"
        ),
    )
    .with_detail(serde_json::json!({ "activated_cutover_id": committed })))
}

/// The project that created an aggregate is the only one that may write to it.
///
/// `ProjectId` is documented as the project an aggregate belongs to, but the
/// aggregate id space is global and no projection row carries a project — so
/// without this, a second project can transition another's task simply by
/// using a key of its own, and the aggregate ends up with a log whose events
/// disagree about who owns it. Any per-project read of that log then returns a
/// partial history of the aggregate and nothing says so.
///
/// The owner is not stored anywhere new: the log already records who created
/// what, so it is the project on the aggregate's first event. An aggregate
/// with no events yet is unowned, which is exactly the create case.
async fn check_aggregate_owner(
    conn: &mut PgConnection,
    envelope: &CommandEnvelope,
    route: &Route,
) -> Result<(), Refusal> {
    // Served by the UNIQUE (aggregate_type, aggregate_id, aggregate_version)
    // index the CAS already needs — no new index, no new column.
    let owner: Option<String> = sqlx::query_scalar(AGGREGATE_OWNER)
        .bind(route.aggregate_type)
        .bind(&route.aggregate_id)
        .fetch_optional(conn)
        .await
        .map_err(|e| Refusal::storage(format!("read aggregate owner: {e}")))?;
    match owner {
        Some(owner) if owner != envelope.project_id.as_str() => Err(Refusal::validation(format!(
            "{}/{} belongs to project {owner:?}, not {:?}",
            route.aggregate_type,
            route.aggregate_id,
            envelope.project_id.as_str()
        ))),
        _ => Ok(()),
    }
}

/// The envelope's `expected_version`, when present, must agree with the version
/// the kernel derived for this command.
///
/// The field is documented as the CAS precondition, and for the five commands
/// whose bodies carry no version of their own it is the ONLY CAS a caller can
/// express — ignoring it turns an intended compare-and-swap into silent
/// last-write-wins. Checking it against the derived version rather than
/// replacing that version keeps one rule for all twenty commands: a create
/// derives 0, a body-CAS command derives what its body already agreed to, and
/// the rest derive what the log holds now.
fn check_expected_version(envelope: &CommandEnvelope, derived: u32) -> Result<(), Refusal> {
    match envelope.expected_version {
        Some(expected) if expected != derived => Err(Refusal::new(
            KernelErrorCode::StaleVersion,
            format!("version conflict: actual {derived}, expected {expected}"),
        )
        .with_detail(serde_json::json!({ "actual": derived, "expected": expected }))),
        _ => Ok(()),
    }
}

/// What this command's key already means.
async fn prior_for_key(
    conn: &mut PgConnection,
    envelope: &CommandEnvelope,
    route: &Route,
    payload: &serde_json::Value,
) -> Result<Prior, Refusal> {
    let stored = events_for_key(
        conn,
        envelope.project_id.as_str(),
        route.aggregate_type,
        &route.aggregate_id,
        envelope.idempotency_key.as_str(),
    )
    .await?;
    let Some(first) = stored.first() else {
        return Ok(Prior::Unused);
    };
    // A replay must be the SAME request. Target and body are the obvious half;
    // the other two are not.
    //
    // `project_id`, because the aggregate namespace is global while idempotency
    // is per-project: a key can be free in this project and already taken on
    // this aggregate by another, and answering that as a replay hands the caller
    // another project's events for a command that never ran.
    //
    // `actor`, because a replay is answered BEFORE `transition::apply` runs, and
    // `apply` is the only thing that enforces the liveness-producer flip rule.
    // Without this the short-circuit becomes what `gwk_domain::transition`
    // documents it must never be: a way to get a guarded edge answered without
    // being authorized for it.
    //
    // Comparing the stored payload is exact because the payload IS the
    // canonical command body — there is no second encoding of it to disagree
    // with.
    let same_request = stored.len() == 1
        && first.project_id == envelope.project_id
        && first.aggregate_type == route.aggregate_type
        && first.aggregate_id.as_str() == route.aggregate_id
        && first.actor == envelope.actor
        && &first.payload == payload;
    if same_request {
        return Ok(Prior::Replay(stored));
    }
    Ok(Prior::Conflict(format!(
        "idempotency key {:?} already names a different request on {}/{} in project {}",
        envelope.idempotency_key.as_str(),
        first.aggregate_type,
        first.aggregate_id,
        first.project_id
    )))
}

/// The version the aggregate must currently be at for this command to apply.
///
/// Three shapes, and which one a command gets is decided by the contract, not
/// by convenience: a create asserts the aggregate does not exist yet, a command
/// carrying `expected_version` gets a CAS against it, and one that carries none
/// takes the log as it stands.
async fn decide(
    conn: &mut PgConnection,
    envelope: &CommandEnvelope,
    command: &KernelCommand,
    route: &Route,
) -> Result<u32, Refusal> {
    use KernelCommand as C;
    Ok(match command {
        // Genesis is version 1, so activation asserts version 1 — which makes
        // the CAS a second, independent proof that the kernel was still sealed
        // at the instant of the append, not merely when the epoch was read.
        C::ActivateKernel { .. } => 1,

        // Version 0 is the assertion "this aggregate has no events" — so the
        // CAS in the append is what refuses a duplicate create, and no separate
        // existence check can drift from it.
        C::CreateTask { .. }
        | C::CreateAttempt { .. }
        | C::OpenEngineSession { .. }
        | C::AcquireLease { .. }
        | C::RegisterWorktree { .. }
        | C::RegisterDispatchNode { .. }
        | C::SendMessage { .. }
        | C::IssueCommand { .. }
        | C::OpenGate { .. }
        | C::RecordEvidence { .. }
        | C::GrantAuthority { .. }
        | C::RaiseAttention { .. } => 0,

        // Neither row carries a version column, so there is no CAS to express:
        // a grant is live until revoked and an item is open until resolved,
        // and both projections refuse the second write themselves — the UPDATE
        // is predicated on the state it expects to find.
        C::RevokeAuthority { .. } | C::ResolveAttention { .. } => {
            current_aggregate_version(conn, route.aggregate_type, &route.aggregate_id).await?
        }

        C::TransitionTask {
            to,
            expected_version,
            ..
        } => {
            let cursor: Cursor<TaskState> =
                fsm_cursor(conn, TASK_CURSOR, &route.aggregate_id, "task").await?;
            decide_transition(&cursor, *to, *expected_version, envelope, None)?
        }
        C::TransitionAttempt {
            to,
            expected_version,
            receipt_id,
            ..
        } => {
            let cursor: Cursor<AttemptState> =
                fsm_cursor(conn, ATTEMPT_CURSOR, &route.aggregate_id, "attempt").await?;
            decide_transition(
                &cursor,
                *to,
                *expected_version,
                envelope,
                receipt_id.as_ref(),
            )?
        }

        C::TransitionMessage {
            to,
            expected_version,
            ..
        } => {
            let cursor: Cursor<MessageState> =
                fsm_cursor(conn, MESSAGE_CURSOR, &route.aggregate_id, "message").await?;
            decide_transition(&cursor, *to, *expected_version, envelope, None)?
        }
        C::TransitionCommand {
            to,
            expected_version,
            ..
        } => {
            // `verification_complete` is reachable only through
            // `RecordCommandOutcome`: the row's CHECK ties that state to an
            // outcome column this command has no value for, so allowing it
            // here would trade a typed refusal for a constraint violation
            // raised from inside the projection.
            if *to == CommandState::VerificationComplete {
                return Err(Refusal::validation(
                    "a command reaches verification_complete by recording its outcome, \
                     which is the same write",
                ));
            }
            let cursor: Cursor<CommandState> =
                fsm_cursor(conn, COMMAND_CURSOR, &route.aggregate_id, "command").await?;
            decide_transition(&cursor, *to, *expected_version, envelope, None)?
        }
        C::RecordCommandOutcome {
            expected_version, ..
        } => {
            // Checked as the edge it is — signaled -> verification_complete —
            // so a command that has not been signaled yet is refused with
            // `illegal_edge` rather than silently completing.
            let cursor: Cursor<CommandState> =
                fsm_cursor(conn, COMMAND_CURSOR, &route.aggregate_id, "command").await?;
            decide_transition(
                &cursor,
                CommandState::VerificationComplete,
                *expected_version,
                envelope,
                None,
            )?
        }

        C::DecideGate {
            expected_version, ..
        } => {
            // A verdict is a value, not an edge: `gwk.gate` has a CHECK on the
            // four verdicts and no transition table, so the contract admits
            // re-deciding one. The version CAS is the whole guard.
            decide_cas(
                conn,
                GATE_VERSION,
                &route.aggregate_id,
                "gate",
                *expected_version,
            )
            .await?
        }

        C::UpdateBudget {
            expected_version, ..
        }
        | C::RecordAttemptOutcome {
            expected_version, ..
        } => {
            decide_cas(
                conn,
                ATTEMPT_VERSION,
                &route.aggregate_id,
                "attempt",
                *expected_version,
            )
            .await?
        }
        C::RenewLease {
            expected_version, ..
        }
        | C::ReleaseLease {
            expected_version, ..
        }
        | C::ExpireLease {
            expected_version, ..
        } => {
            decide_cas(
                conn,
                LEASE_VERSION,
                &route.aggregate_id,
                "lease",
                *expected_version,
            )
            .await?
        }
        C::TransitionDispatchNode {
            expected_version, ..
        } => {
            // Version CAS with no edge check: the spawn tree's lifecycle label
            // is open by design, so there is no edge table to check it against.
            decide_cas(
                conn,
                DISPATCH_NODE_VERSION,
                &route.aggregate_id,
                "dispatch_node",
                *expected_version,
            )
            .await?
        }

        C::WriteOrchestratorCheckpoint { checkpoint } => {
            if let Some(current) = checkpoint_seq(conn, &route.aggregate_id).await?
                && checkpoint.seq.value() <= current
            {
                // A resume cursor, not a counter: re-writing a seq that has
                // already been superseded is how recovery would restart from
                // stale state. Refused here so the caller gets the number back
                // instead of the trigger's exception.
                return Err(Refusal::new(
                    KernelErrorCode::StaleVersion,
                    format!(
                        "checkpoint seq must advance past {current} (got {})",
                        checkpoint.seq
                    ),
                )
                .with_detail(serde_json::json!({
                    "actual": current.to_string(),
                    "presented": checkpoint.seq.to_string(),
                })));
            }
            current_aggregate_version(conn, route.aggregate_type, &route.aggregate_id).await?
        }

        // No CAS in the command, so none is invented: these take the aggregate
        // as the log has it. The single-writer append is what orders them.
        C::CloseEngineSession { .. }
        | C::UpdateWorktree { .. }
        | C::ReleaseWorktree { .. }
        | C::RecordRound { .. }
        | C::RecordFinding { .. } => {
            current_aggregate_version(conn, route.aggregate_type, &route.aggregate_id).await?
        }

        other => {
            return Err(Refusal::validation(format!(
                "{} is not accepted by this kernel yet",
                other.command_type()
            )));
        }
    })
}

/// Read one FSM row into the cursor [`transition::apply`] decides from.
async fn fsm_cursor<S: DeserializeOwned>(
    conn: &mut PgConnection,
    select: &'static str,
    id: &str,
    kind: &str,
) -> Result<Cursor<S>, Refusal> {
    let row = sqlx::query(select)
        .bind(id)
        .fetch_optional(conn)
        .await
        .map_err(|e| Refusal::storage(format!("read {kind} {id}: {e}")))?
        .ok_or_else(|| Refusal::not_found(format!("no {kind} {id}")))?;
    let state: String = row
        .try_get("state")
        .map_err(|e| Refusal::storage(format!("column state: {e}")))?;
    Ok(Cursor {
        state: from_wire_str(&state)?,
        version: row_version(&row)?,
        // A replay was already answered from the log above, so `apply`'s keyed
        // short-circuit has nothing left to decide. Leaving these absent is the
        // honest way to say so — a key here would only ever fail to match.
        applied_idempotency_key: None,
        applied_by: None,
    })
}

fn row_version(row: &sqlx::postgres::PgRow) -> Result<u32, Refusal> {
    let version: i64 = row
        .try_get("version")
        .map_err(|e| Refusal::storage(format!("column version: {e}")))?;
    u32::try_from(version).map_err(|e| Refusal::storage(format!("version out of range: {e}")))
}

/// Run one transition through the domain's single decision function.
///
/// Every refusal it can return is a contract value, so each gets its own wire
/// code rather than collapsing into one "rejected".
fn decide_transition<S>(
    cursor: &Cursor<S>,
    to: S,
    expected_version: u32,
    envelope: &CommandEnvelope,
    receipt_id: Option<&ReceiptId>,
) -> Result<u32, Refusal>
where
    S: transition::TransitionGuard + serde::Serialize,
{
    let request = TransitionRequest {
        to,
        expected_version,
        actor: &envelope.actor,
        idempotency_key: None,
        receipt_id,
    };
    match transition::apply(cursor, &request) {
        TransitionResult::Applied { .. } => Ok(cursor.version),
        TransitionResult::IllegalEdge { from, to } => {
            let (from, to) = (wire_str(&from)?, wire_str(&to)?);
            Err(
                Refusal::new(KernelErrorCode::IllegalEdge, format!("{from} -> {to}"))
                    .with_detail(serde_json::json!({ "from": from, "to": to })),
            )
        }
        TransitionResult::StaleVersion { actual, expected } => Err(Refusal::new(
            KernelErrorCode::StaleVersion,
            format!("version conflict: actual {actual}, expected {expected}"),
        )
        .with_detail(serde_json::json!({ "actual": actual, "expected": expected }))),
        // The liveness-producer flip rule lives here and only here: the DDL
        // guard checks edges and versions, never WHO. A refusal at this arm is
        // the one thing storage cannot catch.
        TransitionResult::UnauthorizedActor { reason } => {
            Err(Refusal::new(KernelErrorCode::Authority, reason))
        }
    }
}

/// Compare a versioned row against the command's `expected_version`.
async fn decide_cas(
    conn: &mut PgConnection,
    select: &'static str,
    id: &str,
    kind: &str,
    expected: u32,
) -> Result<u32, Refusal> {
    let row = sqlx::query(select)
        .bind(id)
        .fetch_optional(conn)
        .await
        .map_err(|e| Refusal::storage(format!("read {kind} {id}: {e}")))?
        .ok_or_else(|| Refusal::not_found(format!("no {kind} {id}")))?;
    let actual = row_version(&row)?;
    if actual != expected {
        return Err(Refusal::new(
            KernelErrorCode::StaleVersion,
            format!("version conflict: actual {actual}, expected {expected}"),
        )
        .with_detail(serde_json::json!({ "actual": actual, "expected": expected })));
    }
    Ok(actual)
}

/// The seq an orchestrator's checkpoint currently holds, if it has one.
async fn checkpoint_seq(
    conn: &mut PgConnection,
    orchestrator_id: &str,
) -> Result<Option<u64>, Refusal> {
    let text: Option<String> = sqlx::query_scalar(CHECKPOINT_SEQ)
        .bind(orchestrator_id)
        .fetch_optional(conn)
        .await
        .map_err(|e| Refusal::storage(format!("read checkpoint {orchestrator_id}: {e}")))?;
    text.map(|text| from_numeric_text(&text))
        .transpose()
        .map_err(|e| Refusal::storage(format!("column seq: {e}")))
}

/// Build the event this command produces.
fn build_event(
    envelope: &CommandEnvelope,
    payload: serde_json::Value,
    route: &Route,
    expected_version: u32,
) -> Result<EventEnvelope, Refusal> {
    let aggregate_version = expected_version.checked_add(1).ok_or_else(|| {
        Refusal::new(
            KernelErrorCode::StaleVersion,
            format!(
                "{}/{} is at the version ceiling",
                route.aggregate_type, route.aggregate_id
            ),
        )
    })?;
    Ok(EventEnvelope {
        // Derived, not minted. `(aggregate_type, aggregate_id,
        // aggregate_version)` is already UNIQUE in the log and the last segment
        // is always digits, so this is collision-free by the constraint that
        // exists anyway — and it is stable under replay, which a random id
        // would not be, with no RNG dependency to make deterministic later.
        event_id: EventId::new(format!(
            "{}:{}:{aggregate_version}",
            route.aggregate_type, route.aggregate_id
        )),
        project_id: envelope.project_id.clone(),
        aggregate_type: route.aggregate_type.to_owned(),
        aggregate_id: AggregateId::new(route.aggregate_id.clone()),
        aggregate_version,
        event_type: route.event_type.to_owned(),
        // The KERNEL's version, not the caller's: the kernel wrote this event,
        // so it is the one making a promise about how to read it. The caller's
        // own declaration was checked on the way in.
        schema_version: ENVELOPE_SCHEMA_VERSION,
        // Both assigned by the store inside the append; the port documents that
        // whatever arrives here is overwritten.
        global_sequence: Seq::new(0),
        appended_at: envelope.issued_at.clone(),
        occurred_at: envelope.issued_at.clone(),
        actor: envelope.actor.clone(),
        origin: envelope.origin.clone(),
        causation_id: envelope.causation_id.clone(),
        correlation_id: envelope.correlation_id.clone(),
        idempotency_key: Some(envelope.idempotency_key.clone()),
        payload,
        payload_ref: None,
    })
}

#[cfg(test)]
mod tests {
    use gwk_domain::ids::{AttemptId, TaskId};

    use super::*;

    fn envelope(command: &KernelCommand) -> CommandEnvelope {
        CommandEnvelope {
            command_id: gwk_domain::ids::CommandId::new("cmd-1"),
            project_id: gwk_domain::ids::ProjectId::new("p"),
            command_type: command.command_type().to_owned(),
            schema_version: ENVELOPE_SCHEMA_VERSION,
            issued_at: gwk_domain::ids::Timestamp::new("2026-07-28T00:00:00Z"),
            actor: gwk_domain::envelope::Actor {
                kind: "kernel".into(),
                id: None,
            },
            origin: gwk_domain::envelope::Origin {
                system: "gw".into(),
                r#ref: None,
            },
            target_aggregate_type: None,
            target_aggregate_id: None,
            expected_version: None,
            idempotency_key: gwk_domain::ids::IdempotencyKey::new("k-1"),
            causation_id: None,
            correlation_id: None,
            payload: serde_json::json!({}),
        }
    }

    #[test]
    fn every_in_scope_command_routes_to_an_aggregate_and_an_event_name() {
        let create = KernelCommand::CreateTask {
            task_id: TaskId::new("t-1"),
            kind: None,
            title: None,
            spec_ref: None,
            project: None,
            priority: None,
            tracker_ref: None,
        };
        let route = route_of(&create).expect("routes");
        assert_eq!(route.aggregate_type, "task");
        assert_eq!(route.aggregate_id, "t-1");
        assert_eq!(route.event_type, "task_created");

        // A round is a fact about the ATTEMPT — it has no aggregate of its own,
        // which is what keeps the attempt's row version equal to its log version.
        let round = KernelCommand::RecordRound {
            attempt_id: AttemptId::new("a-1"),
            round: 2,
            findings: gwk_domain::inherited::RoundFindingSummary {
                total: 0,
                auto_fix: 0,
                ask_user: 0,
                no_op: 0,
            },
        };
        let route = route_of(&round).expect("routes");
        assert_eq!(
            (route.aggregate_type, route.aggregate_id.as_str()),
            ("attempt", "a-1")
        );

        // Evidence is its own aggregate even though its row has no version
        // column: what the log counts and what the projection stores are
        // separate questions.
        let evidence = KernelCommand::RecordEvidence {
            evidence_id: gwk_domain::ids::EvidenceId::new("ev-1"),
            kind: "diff".to_owned(),
            r#ref: "blob://d".to_owned(),
            digest: None,
            byte_size: None,
        };
        let route = route_of(&evidence).expect("routes");
        assert_eq!(
            (route.aggregate_type, route.event_type),
            ("evidence", "evidence_recorded")
        );
    }

    #[test]
    fn a_command_from_a_later_phase_is_refused_by_name() {
        // Not a silent no-op and not a wildcard "unknown command": the caller
        // is told which command was refused, and the kernel never writes an
        // event it has no projection for.
        // `ingest_record` is now the LAST command this kernel does not project.
        // When task 17 lands it there is no out-of-scope command left, and this
        // case should be deleted rather than pointed at something else.
        let ingest = KernelCommand::IngestRecord {
            kind: gwk_domain::ingestion::IngestionKind::Memory,
            payload: serde_json::json!({}),
            payload_ref: None,
        };
        let refusal = route_of(&ingest).expect_err("out of scope");
        assert_eq!(refusal.code, KernelErrorCode::Validation);
        assert!(refusal.message.contains("ingest_record"), "{refusal}");
    }

    #[test]
    fn an_anonymous_checkpoint_has_no_row_to_be_the_latest_of() {
        let anonymous = KernelCommand::WriteOrchestratorCheckpoint {
            checkpoint: gwk_domain::inherited::OrchestratorCheckpoint {
                orchestrator_id: None,
                seq: Seq::new(1),
                native_session_ref: None,
                active_goal: None,
                active_step_ref: None,
                latest_command_ref: None,
                open_attempts: None,
                leases: None,
                pending_approvals: None,
                budget_cursor: None,
            },
        };
        let refusal = route_of(&anonymous).expect_err("no identity");
        assert_eq!(refusal.code, KernelErrorCode::Validation);
    }

    #[test]
    fn an_empty_id_is_refused_before_it_becomes_an_aggregate() {
        let nameless = KernelCommand::CreateTask {
            task_id: TaskId::new(""),
            kind: None,
            title: None,
            spec_ref: None,
            project: None,
            priority: None,
            tracker_ref: None,
        };
        assert_eq!(
            route_of(&nameless).expect_err("empty id").code,
            KernelErrorCode::Validation
        );
    }

    #[test]
    fn the_event_carries_the_kernels_schema_version_and_a_derived_id() {
        let command = KernelCommand::TransitionTask {
            task_id: TaskId::new("t-1"),
            to: TaskState::Working,
            expected_version: 1,
        };
        let route = route_of(&command).expect("routes");
        let payload = serde_json::to_value(&command).expect("serialize");
        let event = build_event(&envelope(&command), payload.clone(), &route, 1).expect("built");
        assert_eq!(event.aggregate_version, 2);
        assert_eq!(event.event_id.as_str(), "task:t-1:2");
        assert_eq!(event.schema_version, ENVELOPE_SCHEMA_VERSION);
        assert_eq!(event.event_type, "task_transitioned");
        // The payload IS the command body — which is what lets the projection
        // applier serve both the live write and a replay from the log.
        assert_eq!(event.payload, payload);
        assert_eq!(
            event.idempotency_key.as_ref().map(|k| k.as_str()),
            Some("k-1")
        );
    }

    #[test]
    fn a_transition_at_the_version_ceiling_is_refused_not_wrapped() {
        let command = KernelCommand::TransitionTask {
            task_id: TaskId::new("t-1"),
            to: TaskState::Working,
            expected_version: u32::MAX,
        };
        let route = route_of(&command).expect("routes");
        let refusal = build_event(&envelope(&command), serde_json::json!({}), &route, u32::MAX)
            .expect_err("the ceiling");
        assert_eq!(refusal.code, KernelErrorCode::StaleVersion);
    }

    #[test]
    fn the_liveness_flip_refusal_is_authority_not_a_version_probe() {
        // The DDL guard checks edges and versions and never WHO, so this arm is
        // the only thing standing between a wrong actor and a blocked flip.
        let command = KernelCommand::TransitionAttempt {
            attempt_id: AttemptId::new("a-1"),
            to: AttemptState::Blocked,
            expected_version: 999,
            receipt_id: None,
        };
        let cursor = Cursor {
            state: AttemptState::Running,
            version: 5,
            applied_idempotency_key: None,
            applied_by: None,
        };
        let refusal = decide_transition(
            &cursor,
            AttemptState::Blocked,
            999,
            &envelope(&command),
            None,
        )
        .expect_err("wrong actor");
        assert_eq!(refusal.code, KernelErrorCode::Authority);
        // And the real version never leaves: the actor refusal answers first.
        assert!(!refusal.message.contains('5'), "{refusal}");
    }

    #[test]
    fn a_refusal_carries_the_number_a_retrier_needs() {
        let command = KernelCommand::TransitionTask {
            task_id: TaskId::new("t-1"),
            to: TaskState::Working,
            expected_version: 1,
        };
        let cursor = Cursor {
            state: TaskState::Submitted,
            version: 4,
            applied_idempotency_key: None,
            applied_by: None,
        };
        let refusal = decide_transition(&cursor, TaskState::Working, 1, &envelope(&command), None)
            .expect_err("stale");
        assert_eq!(refusal.code, KernelErrorCode::StaleVersion);
        assert_eq!(
            refusal.detail,
            Some(serde_json::json!({ "actual": 4, "expected": 1 }))
        );
    }
}
