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
//! Scope is every command in the contract: task, attempt, engine session,
//! lease, worktree, dispatch node, budget, checkpoint, round, finding, message,
//! execution command, gate, evidence, authority grant, attention item, and
//! ingestion. [`route_of`] matches them all WITHOUT a wildcard arm, so a
//! command added to the contract fails to compile here rather than falling into
//! a silent no-op.

use gwk_domain::command::KernelCommand;
use gwk_domain::envelope::{
    CommandEnvelope, ENVELOPE_SCHEMA_VERSION, EventEnvelope, accept_schema_version,
};
use gwk_domain::fsm::{AttemptState, CommandState, MessageState, TaskState};
use gwk_domain::ids::{AggregateId, EventId, ReceiptId, Seq};
use gwk_domain::port::EventStore;
use gwk_domain::protocol::{KernelErrorCode, KernelResult};
use gwk_domain::transition::{self, Cursor, TransitionRequest, TransitionResult};
use hmac::{Hmac, KeyInit as _, Mac};
use serde::de::DeserializeOwned;
use sha2::Sha256;
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
const WORKSPACE_NODE_VERSION: &str = "SELECT version FROM gwk.workspace_node WHERE id = $1";
const WORKSPACE_NODE_KIND: &str = "SELECT kind FROM gwk.workspace_node WHERE id = $1";
const WORKSPACE_NODE_HAS_CHILDREN: &str =
    "SELECT 1 FROM gwk.workspace_node WHERE parent_id = $1 LIMIT 1";
// Walks upward from a candidate parent; a hit on the moved node's own id means
// the move would make the node its own ancestor.
const WORKSPACE_NODE_ANCESTOR: &str = "WITH RECURSIVE ancestors AS ( \
       SELECT id, parent_id FROM gwk.workspace_node WHERE id = $1 \
       UNION ALL \
       SELECT n.id, n.parent_id FROM gwk.workspace_node n \
         JOIN ancestors a ON n.id = a.parent_id \
     ) SELECT 1 FROM ancestors WHERE id = $2 LIMIT 1";
const WORKFLOW_RUN_VERSION: &str = "SELECT version FROM gwk.workflow_run WHERE id = $1";
const WORKFLOW_RUN_STATE: &str = "SELECT state FROM gwk.workflow_run WHERE id = $1";
const PTY_SESSION_VERSION: &str = "SELECT version FROM gwk.pty_session WHERE id = $1";
const PTY_SESSION_STATE: &str = "SELECT state FROM gwk.pty_session WHERE id = $1";
const PTY_SESSION_CURSOR: &str =
    "SELECT state, generation, version FROM gwk.pty_session WHERE id = $1";
const PTY_TEMPLATE_VERSION: &str = "SELECT version FROM gwk.pty_session_template WHERE name = $1";
const PTY_TEMPLATE_STATE: &str = "SELECT state FROM gwk.pty_session_template WHERE name = $1";
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

/// The wire layer needs to know whether a successful input submission is new:
/// a replay must return the original command result without typing the bytes a
/// second time.
pub(crate) struct Submission {
    pub result: KernelResult,
    pub delivery_id: Option<EventId>,
}

/// The ephemeral half of one typed PTY-input request. Invalid wire encodings
/// still reach the store as a refusal so a well-formed send command always
/// leaves its required receipt without moving the bytes into durable state.
pub(crate) enum PtyInputCarrier<'a> {
    Bytes(&'a [u8]),
    Invalid(String),
}

/// Require the ephemeral carrier exactly for the metadata command that names
/// it. This keeps the ordinary submit path from appending a false "sent" event
/// with no bytes behind it, and keeps arbitrary commands from smuggling an
/// unlogged side payload.
fn validate_pty_input_carrier(
    command: &KernelCommand,
    pty_input: Option<&PtyInputCarrier<'_>>,
) -> Result<(), Refusal> {
    match (command, pty_input) {
        (
            KernelCommand::SendPtyInput {
                byte_count,
                pty_session_id,
                ..
            },
            Some(PtyInputCarrier::Bytes(bytes)),
        ) => {
            if bytes.is_empty() {
                return Err(Refusal::validation(format!(
                    "send_pty_input for {pty_session_id} carries an empty batch"
                )));
            }
            if bytes.len() > gwk_domain::protocol::PTY_INPUT_MAX_BYTES {
                return Err(Refusal::validation(format!(
                    "send_pty_input for {pty_session_id} carries {} bytes, over the {}-byte batch bound",
                    bytes.len(),
                    gwk_domain::protocol::PTY_INPUT_MAX_BYTES,
                )));
            }
            let actual = u64::try_from(bytes.len())
                .map_err(|_| Refusal::validation("PTY input size does not fit u64"))?;
            if actual != byte_count.value() {
                return Err(Refusal::validation(format!(
                    "send_pty_input for {pty_session_id} declares {byte_count} bytes but carries {actual}"
                )));
            }
            Ok(())
        }
        (KernelCommand::SendPtyInput { .. }, Some(PtyInputCarrier::Invalid(message))) => {
            Err(Refusal::validation(message.clone()))
        }
        (KernelCommand::SendPtyInput { .. }, None) => Err(Refusal::validation(
            "send_pty_input requires the typed ephemeral byte carrier",
        )),
        (_, Some(_)) => Err(Refusal::validation(
            "the PTY input byte carrier may accompany send_pty_input only",
        )),
        (_, None) => Ok(()),
    }
}

impl PgEventStore {
    /// Apply one command, or answer why not.
    ///
    /// A refusal is a value here, exactly as the contract says: this returns
    /// [`KernelResult::Error`] rather than an `Err`, so the wire layer has
    /// nothing left to translate.
    pub async fn submit(&self, envelope: &CommandEnvelope) -> KernelResult {
        self.submit_inner(envelope, None).await.result
    }

    pub(crate) async fn submit_pty_input_for_delivery(
        &self,
        envelope: &CommandEnvelope,
        carrier: PtyInputCarrier<'_>,
    ) -> Submission {
        self.submit_inner(envelope, Some(carrier)).await
    }

    pub(crate) async fn submit_inner_for_delivery(&self, envelope: &CommandEnvelope) -> Submission {
        self.submit_inner(envelope, None).await
    }

    async fn submit_inner(
        &self,
        envelope: &CommandEnvelope,
        pty_input: Option<PtyInputCarrier<'_>>,
    ) -> Submission {
        match self.try_submit(envelope, pty_input).await {
            Ok(result) => Submission {
                delivery_id: delivery_id(&result),
                result,
            },
            Err(refusal) => Submission {
                delivery_id: None,
                result: refusal.into_result(),
            },
        }
    }

    async fn try_submit(
        &self,
        envelope: &CommandEnvelope,
        pty_input: Option<PtyInputCarrier<'_>>,
    ) -> Result<KernelResult, Refusal> {
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
        let receipted_pty_command = matches!(
            command,
            KernelCommand::SendPtyInput { .. }
                | KernelCommand::ResizePtySession { .. }
                | KernelCommand::StopPtySession { .. }
                | KernelCommand::RequestPtySessionStart { .. }
        );
        let pty_input_command = matches!(command, KernelCommand::SendPtyInput { .. });
        let input_preflight_refusal = if pty_input_command {
            validate_pty_input_carrier(&command, pty_input.as_ref())
                .err()
                .or_else(|| {
                    envelope.expected_version.is_some().then(|| {
                        Refusal::validation(
                            "send_pty_input is generation-guarded and carries no expected_version",
                        )
                    })
                })
        } else {
            validate_pty_input_carrier(&command, pty_input.as_ref())?;
            None
        };
        let route = route_of(envelope, &command)?;
        check_routing(envelope, &route)?;
        check_body_project(envelope, &command)?;
        check_activation(envelope, &command)?;
        check_authority_admin(envelope, &command)?;
        let payload = serde_json::to_value(&command)
            .map_err(|e| Refusal::storage(format!("serialize command body: {e}")))?;
        let input_binding = match pty_input.as_ref() {
            Some(PtyInputCarrier::Bytes(bytes)) => {
                let Some(key) = self.pty_input_binding_key() else {
                    return Err(Refusal::storage(
                        "PTY input replay binding requires the serving kernel's blob key",
                    ));
                };
                let mut mac = Hmac::<Sha256>::new_from_slice(key)
                    .map_err(|_| Refusal::storage("initialize PTY input replay binding"))?;
                mac.update(b"gwk:pty-input-binding:v1\0");
                mac.update(bytes);
                Some(mac.finalize().into_bytes().to_vec())
            }
            Some(PtyInputCarrier::Invalid(_)) | None => None,
        };

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
        if let Some(refusal) = input_preflight_refusal {
            return self
                .receipted_pty_refusal(
                    tx,
                    envelope,
                    &command,
                    &route,
                    (
                        gwk_domain::command::PTY_INPUT_ACTION_CLASS,
                        "request validation",
                    ),
                    refusal,
                )
                .await;
        }

        let expected_version = match prior_for_key(&mut tx, envelope, &route, &payload).await? {
            Prior::Replay(events) => {
                if pty_input_command {
                    let stored: Option<Vec<u8>> = sqlx::query_scalar(
                        "SELECT input_binding FROM gwk_internal.pty_delivery \
                         WHERE project_id = $1 AND idempotency_key = $2",
                    )
                    .bind(envelope.project_id.as_str())
                    .bind(envelope.idempotency_key.as_str())
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|e| Refusal::storage(format!("read PTY input binding: {e}")))?
                    .flatten();
                    if stored.as_deref() != input_binding.as_deref() {
                        return Err(Refusal::new(
                            KernelErrorCode::IdempotencyConflict,
                            format!(
                                "idempotency key {:?} names different PTY input bytes",
                                envelope.idempotency_key.as_str()
                            ),
                        ));
                    }
                }
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
                if receipted_pty_command
                    && sqlx::query_scalar::<_, bool>(
                        "SELECT true FROM gwk_internal.pty_delivery WHERE command_id = $1",
                    )
                    .bind(envelope.command_id.as_str())
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|e| Refusal::storage(format!("check PTY command id: {e}")))?
                    .is_some()
                {
                    return Err(Refusal::new(
                        KernelErrorCode::IdempotencyConflict,
                        format!(
                            "PTY command id {:?} already names another delivery",
                            envelope.command_id.as_str()
                        ),
                    ));
                }
                check_second_cutover(&mut tx, &command, epoch).await?;
                // A start's aggregate id is `{session_id}:{command_id}` — both
                // caller-minted, so it is unpredictable at grant time and no
                // scoped grant could ever match it. Only a class-wide grant
                // would, which would delegate the ENTIRE catalog: exactly the
                // arbitrary-execution surface the declared-template design
                // exists to bound. Authorize a start against its template name
                // instead, so `scope = "review"` grants one template and
                // nothing else. The receipt still names the start aggregate.
                let authority_subject = match &command {
                    KernelCommand::RequestPtySessionStart { template_name, .. } => {
                        template_name.as_str()
                    }
                    _ => route.aggregate_id.as_str(),
                };
                let decision = authority::evaluate(
                    &mut tx,
                    envelope,
                    authority::classification_key(&envelope.command_type, &command),
                    authority_subject,
                )
                .await?;
                if let authority::Decision::Page {
                    action_class,
                    observed_basis,
                } = decision
                {
                    // A page does NOT mutate the target, so this returns before
                    // `decide` — but it still commits, because the receipt and
                    // the attention item are the whole point of paging. This is
                    // the one refusal in the kernel that leaves rows behind.
                    return self
                        .paged(tx, envelope, &command, &route, action_class, observed_basis)
                        .await;
                }
                let authority_result = decision.action_class().zip(decision.observed_basis());
                if !receipted_pty_command
                    && let Some((action_class, observed_basis)) = authority_result
                {
                    write_receipt(
                        &mut tx,
                        &receipt_for(envelope, &route, &command, action_class, observed_basis),
                    )
                    .await?;
                }
                check_attention_dedup(&mut tx, &command).await?;
                let expected_version = match decide(&mut tx, envelope, &command, &route).await {
                    Ok(version) => version,
                    Err(refusal) if receipted_pty_command => {
                        let authority = authority_result.ok_or_else(|| {
                            Refusal::storage("PTY control was not authority-classified")
                        })?;
                        return self
                            .receipted_pty_refusal(
                                tx, envelope, &command, &route, authority, refusal,
                            )
                            .await;
                    }
                    Err(refusal) => return Err(refusal),
                };
                if receipted_pty_command {
                    let (action_class, observed_basis) = authority_result.ok_or_else(|| {
                        Refusal::storage("PTY control was not authority-classified")
                    })?;
                    write_receipt(
                        &mut tx,
                        &receipt_for(envelope, &route, &command, action_class, observed_basis),
                    )
                    .await?;
                }
                expected_version
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
            if receipted_pty_command {
                let event = appended
                    .events
                    .first()
                    .ok_or_else(|| Refusal::storage("a delivered PTY command appended no event"))?;
                sqlx::query(
                    "INSERT INTO gwk_internal.pty_delivery \
                       (project_id, idempotency_key, command_id, event_id, event_seq, input_binding) \
                     VALUES ($1, $2, $3, $4, $5::numeric, $6)",
                )
                .bind(envelope.project_id.as_str())
                .bind(envelope.idempotency_key.as_str())
                .bind(envelope.command_id.as_str())
                .bind(event.event_id.as_str())
                .bind(crate::numeric::to_numeric_text(
                    event.global_sequence.value(),
                ))
                .bind(input_binding.as_deref())
                .execute(&mut *tx)
                .await
                .map_err(|e| Refusal::storage(format!("register PTY delivery: {e}")))?;
            }
        }
        // After the projections, never before: a checkpoint names the sequence
        // it describes, and taking it above this loop would snapshot a state
        // that does not yet include the events it claims. A replay is the only
        // thing that would ever notice, and by then the checkpoint is wrong in
        // a way nothing can repair.
        if let Some(last) = appended.events.last() {
            self.checkpoint_if_due(&mut tx, &writer, last.global_sequence, &last.appended_at)
                .await?;
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
        command: &KernelCommand,
        route: &Route,
        action_class: &'static str,
        observed_basis: &'static str,
    ) -> Result<KernelResult, Refusal> {
        let actor = actor_json(envelope)?;
        let at = envelope.issued_at.as_str();
        let subject = format!("{}/{}", route.aggregate_type, route.aggregate_id);
        write_receipt(
            &mut tx,
            &receipt_for(envelope, route, command, action_class, observed_basis),
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

    /// Commit the one receipt a well-formed input call owes even when its
    /// session lifetime is stale or absent. The target event is not appended.
    async fn receipted_pty_refusal(
        &self,
        mut tx: sqlx::Transaction<'_, sqlx::Postgres>,
        envelope: &CommandEnvelope,
        command: &KernelCommand,
        route: &Route,
        authority: (&str, &str),
        refusal: Refusal,
    ) -> Result<KernelResult, Refusal> {
        let (action_class, basis) = authority;
        let observed_basis = format!("{basis}; refused={}", refusal.code);
        write_receipt(
            &mut tx,
            &receipt_for(envelope, route, command, action_class, &observed_basis),
        )
        .await?;
        tx.commit()
            .await
            .map_err(|e| Refusal::storage(format!("commit input refusal receipt: {e}")))?;
        Err(refusal)
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

fn delivery_id(result: &KernelResult) -> Option<EventId> {
    match result {
        KernelResult::CommandApplied { events, .. } => {
            events.first().map(|event| event.event_id.clone())
        }
        _ => None,
    }
}

fn validate_pty_template(
    name: &str,
    command: &str,
    args: &[String],
    cwd: Option<&str>,
    env: &std::collections::BTreeMap<String, String>,
    cols: u16,
    rows: u16,
) -> Result<(), Refusal> {
    const NAME_MAX: usize = 128;
    const COMMAND_MAX: usize = 4_096;
    const ARGS_MAX: usize = 256;
    const ARG_MAX: usize = 16_384;
    const ENV_MAX: usize = 256;
    const ENV_ITEM_MAX: usize = 16_384;
    const CWD_MAX: usize = 4_096;
    let name = name.trim();
    if name.is_empty()
        || name.len() > NAME_MAX
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(Refusal::validation(
            "a PTY template name must be 1..=128 ASCII letters, digits, '-', '_', or '.'",
        ));
    }
    if command.trim().is_empty() || command.len() > COMMAND_MAX || command.as_bytes().contains(&0) {
        return Err(Refusal::validation(
            "a PTY template requires one bounded non-NUL command",
        ));
    }
    if args.len() > ARGS_MAX
        || args
            .iter()
            .any(|arg| arg.len() > ARG_MAX || arg.as_bytes().contains(&0))
    {
        return Err(Refusal::validation(
            "PTY template arguments exceed their bounded non-NUL shape",
        ));
    }
    if cwd.is_some_and(|cwd| cwd.is_empty() || cwd.len() > CWD_MAX || cwd.as_bytes().contains(&0)) {
        return Err(Refusal::validation(
            "a PTY template cwd must be bounded and non-NUL",
        ));
    }
    if env.len() > ENV_MAX
        || env.iter().any(|(key, value)| {
            key.is_empty()
                || key.contains('=')
                || key.len() > ENV_ITEM_MAX
                || value.len() > ENV_ITEM_MAX
                || key.as_bytes().contains(&0)
                || value.as_bytes().contains(&0)
        })
    {
        return Err(Refusal::validation(
            "PTY template environment exceeds its bounded non-NUL shape",
        ));
    }
    // The reference discipline covers env VALUES and only them. `command`,
    // `args`, and `cwd` above get shape validation and are then persisted
    // verbatim and permanently — the event log is append-only and the runtime
    // role holds no UPDATE or DELETE on it, so a secret written there can never
    // be redacted. The refusal says so, because the operator meets this rule at
    // the point of use and nowhere else.
    if env.values().any(|value| {
        value
            .strip_prefix("env:")
            .is_none_or(|name| !valid_environment_name(name))
    }) {
        return Err(Refusal::validation(
            "PTY template environment values must be env:NAME references, not persisted values \
             — note that command, args, and cwd are persisted verbatim and unredactably, so a \
             credential must travel as an env:NAME reference, never as an argument",
        ));
    }
    validate_pty_grid(cols, rows, "a PTY template")?;
    Ok(())
}

fn valid_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        && name.len() <= 256
}

fn validate_pty_grid(cols: u16, rows: u16, subject: &str) -> Result<(), Refusal> {
    if !gwk_domain::pty_grid_dimensions_are_bounded(cols, rows) {
        return Err(Refusal::validation(format!(
            "{subject} requires non-zero dimensions no larger than {} per axis and {} cells; got {cols}x{rows}",
            gwk_domain::PTY_GRID_MAX_AXIS,
            gwk_domain::PTY_GRID_MAX_CELLS,
        )));
    }
    Ok(())
}

/// Which aggregate a command addresses, and the event it produces there.
///
/// One match rather than two: an aggregate without an event name (or the
/// reverse) would be a routing bug that only shows up in the log.
fn route_of(envelope: &CommandEnvelope, command: &KernelCommand) -> Result<Route, Refusal> {
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
        C::RecordAttemptRuntime { attempt_id, .. } => (
            "attempt",
            attempt_id.as_str().to_owned(),
            "attempt_runtime_recorded",
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

        // The tree-shape rules that need no row lookup are refused here;
        // decide() holds the ones that must read the parent. The table's
        // triggers stay the backstop, not the error path.
        C::CreateWorkspaceNode {
            workspace_node_id,
            kind,
            parent_id,
            session_id,
        } => {
            use gwk_domain::entity::WorkspaceNodeKind as K;
            if session_id.is_some() && *kind != K::Pane {
                return Err(Refusal::validation(format!(
                    "a {} carries no session binding — only a pane does",
                    kind.as_str()
                )));
            }
            match (kind, parent_id) {
                (K::Workspace, Some(_)) => {
                    return Err(Refusal::validation(
                        "a workspace is a root and cannot have a parent".to_owned(),
                    ));
                }
                (K::Tab | K::Pane, None) => {
                    return Err(Refusal::validation(format!(
                        "a {} must sit under a parent",
                        kind.as_str()
                    )));
                }
                _ => {}
            }
            if parent_id.as_ref() == Some(workspace_node_id) {
                return Err(Refusal::validation(
                    "a node cannot parent itself".to_owned(),
                ));
            }
            (
                "workspace_node",
                workspace_node_id.as_str().to_owned(),
                "workspace_node_created",
            )
        }
        C::MoveWorkspaceNode {
            workspace_node_id,
            parent_id,
            ..
        } => {
            if parent_id == workspace_node_id {
                return Err(Refusal::validation(
                    "a node cannot parent itself".to_owned(),
                ));
            }
            (
                "workspace_node",
                workspace_node_id.as_str().to_owned(),
                "workspace_node_moved",
            )
        }
        C::RebindWorkspacePane {
            workspace_node_id, ..
        } => (
            "workspace_node",
            workspace_node_id.as_str().to_owned(),
            "workspace_pane_rebound",
        ),
        C::CloseWorkspaceNode {
            workspace_node_id, ..
        } => (
            "workspace_node",
            workspace_node_id.as_str().to_owned(),
            "workspace_node_closed",
        ),

        // The step's VALUE is template data (decision 17) — the kernel checks
        // shape only, never the name.
        C::OpenWorkflowRun {
            workflow_run_id,
            template_ref,
            template_sha256,
            ..
        } => {
            if template_ref.trim().is_empty() {
                return Err(Refusal::validation(
                    "a workflow run must name the template it follows".to_owned(),
                ));
            }
            if let Some(digest) = template_sha256
                && !gwk_domain::is_sha256_hex(digest)
            {
                return Err(Refusal::validation(
                    "template_sha256 must be 64 lowercase hex characters".to_owned(),
                ));
            }
            (
                "workflow_run",
                workflow_run_id.as_str().to_owned(),
                "workflow_run_opened",
            )
        }
        C::AdvanceWorkflowRun {
            workflow_run_id,
            step,
            ..
        } => {
            if step.trim().is_empty() {
                return Err(Refusal::validation(
                    "a workflow run advances to a named step".to_owned(),
                ));
            }
            (
                "workflow_run",
                workflow_run_id.as_str().to_owned(),
                "workflow_run_advanced",
            )
        }
        C::CloseWorkflowRun {
            workflow_run_id,
            outcome,
            ..
        } => {
            if !matches!(outcome.as_str(), "completed" | "failed" | "canceled") {
                return Err(Refusal::validation(format!(
                    "a workflow run closes as completed, failed, or canceled — not {outcome:?}"
                )));
            }
            (
                "workflow_run",
                workflow_run_id.as_str().to_owned(),
                "workflow_run_closed",
            )
        }

        C::OpenPtySession {
            pty_session_id,
            generation,
            ..
        } => {
            if generation.as_str().trim().is_empty() {
                return Err(Refusal::validation(
                    "a pty session opens under a named host generation".to_owned(),
                ));
            }
            (
                "pty_session",
                pty_session_id.as_str().to_owned(),
                "pty_session_opened",
            )
        }
        C::RecordPtyAttach { pty_session_id, .. } => (
            "pty_session",
            pty_session_id.as_str().to_owned(),
            "pty_attach_recorded",
        ),
        C::RecordPtyDetach { pty_session_id, .. } => (
            "pty_session",
            pty_session_id.as_str().to_owned(),
            "pty_detach_recorded",
        ),
        C::SendPtyInput {
            pty_session_id,
            generation,
            ..
        } => (
            "pty_session",
            gwk_domain::ids::pty_session_lifetime_id(pty_session_id, generation)
                .as_str()
                .to_owned(),
            "pty_input_requested",
        ),
        C::ResizePtySession {
            pty_session_id,
            generation,
            cols,
            rows,
        } => {
            validate_pty_grid(*cols, *rows, "a PTY resize")?;
            (
                "pty_session",
                gwk_domain::ids::pty_session_lifetime_id(pty_session_id, generation)
                    .as_str()
                    .to_owned(),
                "pty_resize_requested",
            )
        }
        C::StopPtySession {
            pty_session_id,
            generation,
        } => (
            "pty_session",
            gwk_domain::ids::pty_session_lifetime_id(pty_session_id, generation)
                .as_str()
                .to_owned(),
            "pty_stop_requested",
        ),
        C::ClosePtySession { pty_session_id, .. } => (
            "pty_session",
            pty_session_id.as_str().to_owned(),
            "pty_session_closed",
        ),
        C::DeclarePtySessionTemplate {
            template_name,
            command,
            args,
            cwd,
            env,
            cols,
            rows,
        } => {
            validate_pty_template(
                template_name.as_str(),
                command,
                args,
                cwd.as_deref(),
                env,
                *cols,
                *rows,
            )?;
            (
                "pty_session_template",
                template_name.as_str().to_owned(),
                "pty_session_template_declared",
            )
        }
        C::RetirePtySessionTemplate { template_name, .. } => (
            "pty_session_template",
            template_name.as_str().to_owned(),
            "pty_session_template_retired",
        ),
        C::RequestPtySessionStart {
            template_name,
            pty_session_id,
        } => {
            if template_name.as_str().trim().is_empty() {
                return Err(Refusal::validation("a PTY start names no template"));
            }
            (
                "pty_session_start",
                format!("{pty_session_id}:{}", envelope.command_id),
                "pty_session_start_requested",
            )
        }

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
        // Its own aggregate on purpose: a spend fact must never contend with
        // the FSM CAS of the attempt it describes.
        C::RecordCostEntry {
            cost_entry_id,
            attempt_id,
            engine_session_id,
            dispatch_node_id,
            ..
        } => {
            // The table constrains this too; refusing here keeps the storage
            // layer's CHECK a backstop instead of the error path.
            if attempt_id.is_none() && engine_session_id.is_none() && dispatch_node_id.is_none() {
                return Err(Refusal::validation(
                    "a cost entry that names no attempt, engine session, or dispatch node \
                     accrues to nothing"
                        .to_owned(),
                ));
            }
            (
                "cost_entry",
                cost_entry_id.as_str().to_owned(),
                "cost_entry_recorded",
            )
        }

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
        // Their own event types, not a flavour of `attention_resolved`: the log
        // is what proves what the operator actually did, and "quieted" and
        // "handled" are different claims about the same row.
        C::AckAttention {
            attention_item_id, ..
        } => (
            "attention_item",
            attention_item_id.as_str().to_owned(),
            "attention_acked",
        ),
        C::MuteAttention {
            attention_item_id, ..
        } => (
            "attention_item",
            attention_item_id.as_str().to_owned(),
            "attention_muted",
        ),
        // Its own event, not an `attention_muted` carrying a deadline already
        // past. Both reach the same row state; only one of them SAYS what the
        // operator meant, and in an append-only ledger that difference is the
        // whole point of writing the event down.
        C::UnmuteAttention {
            attention_item_id, ..
        } => (
            "attention_item",
            attention_item_id.as_str().to_owned(),
            "attention_unmuted",
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

        // The one command whose aggregate id the caller does not supply — an
        // ingested record has no name of its own, and `gw ingest submit --kind
        // <kind> --file <path|->` offers nowhere to put one. Deriving it from
        // the `(project_id, idempotency_key)` pair the envelope already had to
        // carry is what `receipt_for` does for the same reason, and it is what
        // makes a retried ingest resolve to the SAME record rather than a
        // second copy of the same reading: the retry presents the same key, so
        // it lands on the same aggregate and the log answers it as a replay.
        C::IngestRecord { .. } => (
            "ingested_record",
            format!(
                "ingest:{}:{}",
                envelope.project_id.as_str(),
                envelope.idempotency_key.as_str()
            ),
            "record_ingested",
        ),
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
    command: &KernelCommand,
    action_class: &str,
    observed_basis: &str,
) -> gwk_domain::entity::Receipt {
    let observed_basis = match command {
        KernelCommand::SendPtyInput { byte_count, .. } => {
            format!("{observed_basis}; byte_count={byte_count}")
        }
        KernelCommand::ResizePtySession { cols, rows, .. } => {
            format!("{observed_basis}; cols={cols}; rows={rows}")
        }
        KernelCommand::StopPtySession { .. } => format!("{observed_basis}; stop=true"),
        KernelCommand::RequestPtySessionStart { template_name, .. } => {
            format!("{observed_basis}; template={template_name}")
        }
        _ => observed_basis.to_owned(),
    };
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
        observed_basis: Some(observed_basis),
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

/// Authority administration is an operator act. The UDS peer credential is
/// the process boundary; this logical actor check prevents a conforming
/// orchestrator from granting or revoking its own standing authority.
fn check_authority_admin(
    envelope: &CommandEnvelope,
    command: &KernelCommand,
) -> Result<(), Refusal> {
    if matches!(
        command,
        KernelCommand::GrantAuthority { .. }
            | KernelCommand::RevokeAuthority { .. }
            | KernelCommand::DeclarePtySessionTemplate { .. }
            | KernelCommand::RetirePtySessionTemplate { .. }
    ) && envelope.actor.kind != "operator"
    {
        return Err(Refusal::new(
            KernelErrorCode::Authority,
            format!(
                "{} requires actor kind operator, not {:?}",
                command.command_type(),
                envelope.actor.kind
            ),
        ));
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

        // The tree rules that read another row are decided here, before the
        // append; the parentage trigger in the projection is the backstop.
        // Version 0 on create for the same reason as the block below: the
        // aggregate must not exist yet, and the append's CAS proves it.
        C::CreateWorkspaceNode {
            kind, parent_id, ..
        } => {
            if let Some(parent) = parent_id {
                assert_legal_parent(conn, *kind, parent.as_str()).await?;
            }
            0
        }
        C::MoveWorkspaceNode {
            workspace_node_id,
            parent_id,
            expected_version,
        } => {
            let kind = workspace_node_kind(conn, workspace_node_id.as_str())
                .await?
                .ok_or_else(|| {
                    Refusal::validation(format!(
                        "workspace_node {workspace_node_id} does not exist"
                    ))
                })?;
            if kind == "workspace" {
                return Err(Refusal::validation(
                    "a workspace is a root and cannot be moved under a parent".to_owned(),
                ));
            }
            let kind = match kind.as_str() {
                "tab" => gwk_domain::entity::WorkspaceNodeKind::Tab,
                _ => gwk_domain::entity::WorkspaceNodeKind::Pane,
            };
            assert_legal_parent(conn, kind, parent_id.as_str()).await?;
            // A hit means the moved node sits above its would-be parent, so
            // the move would close a cycle and orphan the subtree from every
            // root — refused with both ids so the caller can see the loop.
            let cycle = sqlx::query(WORKSPACE_NODE_ANCESTOR)
                .bind(parent_id.as_str())
                .bind(workspace_node_id.as_str())
                .fetch_optional(&mut *conn)
                .await
                .map_err(|e| Refusal::storage(format!("walk workspace ancestry: {e}")))?;
            if cycle.is_some() {
                return Err(Refusal::validation(format!(
                    "moving {workspace_node_id} under {parent_id} would make it its own ancestor"
                )));
            }
            decide_cas(
                conn,
                WORKSPACE_NODE_VERSION,
                &route.aggregate_id,
                "workspace_node",
                *expected_version,
            )
            .await?
        }
        C::RebindWorkspacePane {
            workspace_node_id,
            expected_version,
            ..
        } => {
            match workspace_node_kind(conn, workspace_node_id.as_str()).await? {
                None => {
                    return Err(Refusal::validation(format!(
                        "workspace_node {workspace_node_id} does not exist"
                    )));
                }
                Some(kind) if kind != "pane" => {
                    return Err(Refusal::validation(format!(
                        "a {kind} carries no session binding — only a pane does"
                    )));
                }
                Some(_) => {}
            }
            decide_cas(
                conn,
                WORKSPACE_NODE_VERSION,
                &route.aggregate_id,
                "workspace_node",
                *expected_version,
            )
            .await?
        }
        C::CloseWorkspaceNode {
            workspace_node_id,
            expected_version,
        } => {
            // Close is a real DELETE in the projection, so a node with live
            // children must not leave first — the subtree closes bottom-up.
            let child = sqlx::query(WORKSPACE_NODE_HAS_CHILDREN)
                .bind(workspace_node_id.as_str())
                .fetch_optional(&mut *conn)
                .await
                .map_err(|e| Refusal::storage(format!("check workspace children: {e}")))?;
            if child.is_some() {
                return Err(Refusal::validation(format!(
                    "workspace_node {workspace_node_id} still has children — close them first"
                )));
            }
            decide_cas(
                conn,
                WORKSPACE_NODE_VERSION,
                &route.aggregate_id,
                "workspace_node",
                *expected_version,
            )
            .await?
        }

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
        | C::RecordCostEntry { .. }
        | C::OpenWorkflowRun { .. }
        | C::OpenPtySession { .. }
        | C::DeclarePtySessionTemplate { .. }
        | C::GrantAuthority { .. }
        | C::RaiseAttention { .. } => 0,

        // A run leaves `running` exactly once; both verbs read the state under
        // the writer lock so a closed run refuses before the CAS can even see
        // a stale version. The no-delete trigger keeps the row around to say so.
        C::AdvanceWorkflowRun {
            workflow_run_id,
            expected_version,
            ..
        }
        | C::CloseWorkflowRun {
            workflow_run_id,
            expected_version,
            ..
        } => {
            match workflow_run_state(conn, workflow_run_id.as_str()).await? {
                None => {
                    return Err(Refusal::not_found(format!(
                        "no workflow_run {workflow_run_id}"
                    )));
                }
                Some(state) if state != "running" => {
                    return Err(Refusal::validation(format!(
                        "workflow_run {workflow_run_id} already closed as {state}"
                    )));
                }
                Some(_) => {}
            }
            decide_cas(
                conn,
                WORKFLOW_RUN_VERSION,
                &route.aggregate_id,
                "workflow_run",
                *expected_version,
            )
            .await?
        }

        // A pty session leaves `running` exactly once, and an attach requires
        // a session that can still be attached: both verbs read the state
        // under the writer lock so a closed session refuses before the CAS
        // can see a stale version. The no-delete trigger keeps the row for
        // the receipt.
        C::RecordPtyAttach {
            pty_session_id,
            expected_version,
        } => {
            match pty_session_state(conn, pty_session_id.as_str()).await? {
                None => {
                    return Err(Refusal::not_found(format!(
                        "no pty_session {pty_session_id}"
                    )));
                }
                Some(state) if state != "running" => {
                    return Err(Refusal::validation(format!(
                        "pty_session {pty_session_id} is {state} and cannot be attached"
                    )));
                }
                Some(_) => {}
            }
            decide_cas(
                conn,
                PTY_SESSION_VERSION,
                &route.aggregate_id,
                "pty_session",
                *expected_version,
            )
            .await?
        }
        C::ClosePtySession {
            pty_session_id,
            expected_version,
        } => {
            match pty_session_state(conn, pty_session_id.as_str()).await? {
                None => {
                    return Err(Refusal::not_found(format!(
                        "no pty_session {pty_session_id}"
                    )));
                }
                Some(state) if !matches!(state.as_str(), "running" | "stopping") => {
                    return Err(Refusal::validation(format!(
                        "pty_session {pty_session_id} already closed as {state}"
                    )));
                }
                Some(_) => {}
            }
            decide_cas(
                conn,
                PTY_SESSION_VERSION,
                &route.aggregate_id,
                "pty_session",
                *expected_version,
            )
            .await?
        }

        // Input is guarded by the host generation rather than by a caller-read
        // projection version. The lifetime row is `{id}:{generation}`; a closed
        // row is therefore a stale generation, not a generic illegal state.
        C::SendPtyInput {
            pty_session_id,
            generation,
            ..
        }
        | C::ResizePtySession {
            pty_session_id,
            generation,
            ..
        }
        | C::StopPtySession {
            pty_session_id,
            generation,
        } => {
            let Some((state, stored_generation, version)) =
                pty_session_cursor(conn, &route.aggregate_id).await?
            else {
                return Err(Refusal::not_found(format!(
                    "no live pty_session {pty_session_id}:{generation}"
                )));
            };
            let state_is_admissible = state == "running"
                || (matches!(command, C::StopPtySession { .. }) && state == "stopping");
            if stored_generation != generation.as_str() || !state_is_admissible {
                return Err(Refusal::new(
                    KernelErrorCode::StaleVersion,
                    format!(
                        "pty session {pty_session_id}:{generation} is stale; ledger state is {state} under generation {stored_generation}"
                    ),
                )
                .with_detail(serde_json::json!({
                    "presented_generation": generation.as_str(),
                    "stored_generation": stored_generation,
                    "state": state,
                })));
            }
            version
        }

        C::RetirePtySessionTemplate {
            template_name,
            expected_version,
        } => {
            match pty_template_state(conn, template_name.as_str()).await? {
                None => {
                    return Err(Refusal::not_found(format!(
                        "no pty_session_template {template_name}"
                    )));
                }
                Some(state) if state != "active" => {
                    return Err(Refusal::validation(format!(
                        "pty_session_template {template_name} already retired"
                    )));
                }
                Some(_) => {}
            }
            decide_cas(
                conn,
                PTY_TEMPLATE_VERSION,
                &route.aggregate_id,
                "pty_session_template",
                *expected_version,
            )
            .await?
        }

        C::RequestPtySessionStart { template_name, .. } => {
            match pty_template_state(conn, template_name.as_str()).await? {
                None => {
                    return Err(Refusal::not_found(format!(
                        "no pty_session_template {template_name}"
                    )));
                }
                Some(state) if state != "active" => {
                    return Err(Refusal::validation(format!(
                        "pty_session_template {template_name} is retired"
                    )));
                }
                Some(_) => 0,
            }
        }

        // A detach is TRUE HISTORY even on a closed row: a retire drops the
        // broadcast sender and the live attach streams end AFTER the close
        // lands, so refusing here would systematically undercount the S8
        // receipt's detach figure. Existence is still required.
        C::RecordPtyDetach {
            pty_session_id,
            expected_version,
        } => {
            if pty_session_state(conn, pty_session_id.as_str())
                .await?
                .is_none()
            {
                return Err(Refusal::not_found(format!(
                    "no pty_session {pty_session_id}"
                )));
            }
            decide_cas(
                conn,
                PTY_SESSION_VERSION,
                &route.aggregate_id,
                "pty_session",
                *expected_version,
            )
            .await?
        }

        // Neither row carries a version column, so there is no CAS to express:
        // a grant is live until revoked and an item is open until resolved,
        // and both projections refuse the second write themselves — the UPDATE
        // is predicated on the state it expects to find.
        //
        // The three quieting verbs join them for a different reason worth
        // keeping straight: they are idempotent last-write-wins stamps, so
        // there is no second write to refuse. Pressing the key twice moves the
        // stamp, and a CAS here would turn that into a stale-version error on
        // an act the operator is entitled to repeat.
        C::RevokeAuthority { .. }
        | C::ResolveAttention { .. }
        | C::AckAttention { .. }
        | C::MuteAttention { .. }
        | C::UnmuteAttention { .. } => {
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
        | C::RecordAttemptRuntime {
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

        // An ingested record's aggregate holds exactly one event, always: its id
        // is derived from the idempotency key, so a second command bearing that
        // key was already answered above as a replay or refused as a conflict
        // and never reaches here. Reading the version anyway — rather than
        // hard-coding the 0 that must be true — keeps the one place that decides
        // what a version means the same for every command.
        C::IngestRecord { .. } => {
            current_aggregate_version(conn, route.aggregate_type, &route.aggregate_id).await?
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

/// The kind column of one workspace node, or `None` when no such row exists.
async fn workspace_node_kind(conn: &mut PgConnection, id: &str) -> Result<Option<String>, Refusal> {
    sqlx::query_scalar::<_, String>(WORKSPACE_NODE_KIND)
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| Refusal::storage(format!("read workspace_node kind: {e}")))
}

/// The state column of one pty session, or `None` when no such row exists.
async fn pty_session_state(conn: &mut PgConnection, id: &str) -> Result<Option<String>, Refusal> {
    sqlx::query_scalar::<_, String>(PTY_SESSION_STATE)
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| Refusal::storage(format!("read pty_session state: {e}")))
}

async fn pty_template_state(
    conn: &mut PgConnection,
    name: &str,
) -> Result<Option<String>, Refusal> {
    sqlx::query_scalar::<_, String>(PTY_TEMPLATE_STATE)
        .bind(name)
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| Refusal::storage(format!("read pty_session_template state: {e}")))
}

/// State, generation, and CAS version for one PTY lifetime row.
async fn pty_session_cursor(
    conn: &mut PgConnection,
    id: &str,
) -> Result<Option<(String, String, u32)>, Refusal> {
    let Some(row) = sqlx::query(PTY_SESSION_CURSOR)
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| Refusal::storage(format!("read pty_session cursor: {e}")))?
    else {
        return Ok(None);
    };
    let state = row
        .try_get("state")
        .map_err(|e| Refusal::storage(format!("pty_session state: {e}")))?;
    let generation = row
        .try_get("generation")
        .map_err(|e| Refusal::storage(format!("pty_session generation: {e}")))?;
    Ok(Some((state, generation, row_version(&row)?)))
}

/// The state column of one workflow run, or `None` when no such row exists.
async fn workflow_run_state(conn: &mut PgConnection, id: &str) -> Result<Option<String>, Refusal> {
    sqlx::query_scalar::<_, String>(WORKFLOW_RUN_STATE)
        .bind(id)
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| Refusal::storage(format!("read workflow_run state: {e}")))
}

/// The parentage table the projection's trigger also enforces — refused here
/// first so the trigger stays a backstop instead of the error path.
async fn assert_legal_parent(
    conn: &mut PgConnection,
    kind: gwk_domain::entity::WorkspaceNodeKind,
    parent_id: &str,
) -> Result<(), Refusal> {
    use gwk_domain::entity::WorkspaceNodeKind as K;
    let parent_kind = workspace_node_kind(conn, parent_id).await?.ok_or_else(|| {
        Refusal::validation(format!("parent workspace_node {parent_id} does not exist"))
    })?;
    let legal = match kind {
        // Creates refuse a parented workspace before reaching here.
        K::Workspace => false,
        K::Tab => parent_kind == "workspace",
        K::Pane => parent_kind == "tab" || parent_kind == "pane",
    };
    if legal {
        Ok(())
    } else {
        Err(Refusal::validation(format!(
            "a {} must not sit under a {parent_kind}",
            kind.as_str()
        )))
    }
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
    use gwk_domain::ids::{AttemptId, ByteCount, PtySessionGeneration, PtySessionId, TaskId};

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
        let route = route_of(&envelope(&create), &create).expect("routes");
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
        let route = route_of(&envelope(&round), &round).expect("routes");
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
        let route = route_of(&envelope(&evidence), &evidence).expect("routes");
        assert_eq!(
            (route.aggregate_type, route.event_type),
            ("evidence", "evidence_recorded")
        );
    }

    #[test]
    fn pty_input_requires_one_matching_bounded_ephemeral_carrier() {
        let command = KernelCommand::SendPtyInput {
            pty_session_id: PtySessionId::new("pty-1"),
            generation: PtySessionGeneration::new("gen-1"),
            byte_count: ByteCount::new(3),
        };
        let matching = PtyInputCarrier::Bytes(b"abc");
        validate_pty_input_carrier(&command, Some(&matching)).expect("matching carrier");

        let empty = PtyInputCarrier::Bytes(b"");
        let short = PtyInputCarrier::Bytes(b"ab");
        for (carrier, expected) in [
            (None, "requires the typed ephemeral byte carrier"),
            (Some(&empty), "empty batch"),
            (Some(&short), "declares 3 bytes but carries 2"),
        ] {
            let refusal = validate_pty_input_carrier(&command, carrier).expect_err("refused");
            assert_eq!(refusal.code, KernelErrorCode::Validation);
            assert!(refusal.message.contains(expected), "{refusal}");
        }

        let oversized = vec![0; gwk_domain::protocol::PTY_INPUT_MAX_BYTES + 1];
        let oversized_command = KernelCommand::SendPtyInput {
            pty_session_id: PtySessionId::new("pty-1"),
            generation: PtySessionGeneration::new("gen-1"),
            byte_count: ByteCount::new(oversized.len() as u64),
        };
        let oversized = PtyInputCarrier::Bytes(&oversized);
        let refusal = validate_pty_input_carrier(&oversized_command, Some(&oversized))
            .expect_err("oversized carrier");
        assert_eq!(refusal.code, KernelErrorCode::Validation);
        assert!(refusal.message.contains("over the"), "{refusal}");

        let other = KernelCommand::CreateTask {
            task_id: TaskId::new("t-1"),
            kind: None,
            title: None,
            spec_ref: None,
            project: None,
            priority: None,
            tracker_ref: None,
        };
        let refusal = validate_pty_input_carrier(&other, Some(&matching))
            .expect_err("only input commands carry bytes");
        assert_eq!(refusal.code, KernelErrorCode::Validation);
    }

    #[test]
    fn pty_input_routes_to_the_exact_session_lifetime() {
        let command = KernelCommand::SendPtyInput {
            pty_session_id: PtySessionId::new("console"),
            generation: PtySessionGeneration::new("7:3"),
            byte_count: ByteCount::new(1),
        };
        let route = route_of(&envelope(&command), &command).expect("routes");
        assert_eq!(route.aggregate_type, "pty_session");
        assert_eq!(route.aggregate_id, "console:7:3");
        assert_eq!(route.event_type, "pty_input_requested");
    }

    #[test]
    fn workspace_tree_shape_rules_refuse_before_any_row_is_read() {
        use gwk_domain::{PtySessionId, WorkspaceNodeId, WorkspaceNodeKind};

        // Each command breaks one structural rule a route needs no database
        // to see: a session on a non-pane, a parented workspace, a floating
        // tab, and the two self-parent spellings.
        let commands = [
            KernelCommand::CreateWorkspaceNode {
                workspace_node_id: WorkspaceNodeId::new("tab-1"),
                kind: WorkspaceNodeKind::Tab,
                parent_id: Some(WorkspaceNodeId::new("ws-1")),
                session_id: Some(PtySessionId::new("pty-1")),
            },
            KernelCommand::CreateWorkspaceNode {
                workspace_node_id: WorkspaceNodeId::new("ws-1"),
                kind: WorkspaceNodeKind::Workspace,
                parent_id: Some(WorkspaceNodeId::new("ws-0")),
                session_id: None,
            },
            KernelCommand::CreateWorkspaceNode {
                workspace_node_id: WorkspaceNodeId::new("tab-1"),
                kind: WorkspaceNodeKind::Tab,
                parent_id: None,
                session_id: None,
            },
            KernelCommand::CreateWorkspaceNode {
                workspace_node_id: WorkspaceNodeId::new("pane-1"),
                kind: WorkspaceNodeKind::Pane,
                parent_id: Some(WorkspaceNodeId::new("pane-1")),
                session_id: None,
            },
            KernelCommand::MoveWorkspaceNode {
                workspace_node_id: WorkspaceNodeId::new("pane-1"),
                parent_id: WorkspaceNodeId::new("pane-1"),
                expected_version: 1,
            },
        ];

        for command in commands {
            let refusal = route_of(&envelope(&command), &command).expect_err("shape rule");
            assert_eq!(refusal.code, KernelErrorCode::Validation, "{refusal:?}");
        }
    }

    #[test]
    fn a_legal_workspace_command_routes_to_the_workspace_aggregate() {
        use gwk_domain::{PtySessionId, WorkspaceNodeId, WorkspaceNodeKind};

        let command = KernelCommand::CreateWorkspaceNode {
            workspace_node_id: WorkspaceNodeId::new("pane-1"),
            kind: WorkspaceNodeKind::Pane,
            parent_id: Some(WorkspaceNodeId::new("tab-1")),
            session_id: Some(PtySessionId::new("pty-1")),
        };
        let route = route_of(&envelope(&command), &command).expect("legal shape");
        assert_eq!(route.aggregate_type, "workspace_node");
        assert_eq!(route.aggregate_id, "pane-1");
        assert_eq!(route.event_type, "workspace_node_created");
    }

    #[test]
    fn an_ingested_record_is_named_by_the_envelope_that_carried_it() {
        // The only command whose aggregate id the caller does not supply. Two
        // submissions differing ONLY in idempotency key are two records; the
        // same key twice is one, which is what makes a retried ingest safe.
        let ingest = KernelCommand::IngestRecord {
            kind: gwk_domain::ingestion::IngestionKind::Memory,
            payload: serde_json::json!({ "text": "recalled" }),
            payload_ref: None,
        };
        let route = route_of(&envelope(&ingest), &ingest).expect("routes");
        assert_eq!(
            (
                route.aggregate_type,
                route.aggregate_id.as_str(),
                route.event_type
            ),
            ("ingested_record", "ingest:p:k-1", "record_ingested")
        );

        let mut second = envelope(&ingest);
        second.idempotency_key = gwk_domain::ids::IdempotencyKey::new("k-2");
        assert_eq!(
            route_of(&second, &ingest).expect("routes").aggregate_id,
            "ingest:p:k-2"
        );
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
        let refusal = route_of(&envelope(&anonymous), &anonymous).expect_err("no identity");
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
            route_of(&envelope(&nameless), &nameless)
                .expect_err("empty id")
                .code,
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
        let route = route_of(&envelope(&command), &command).expect("routes");
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
        let route = route_of(&envelope(&command), &command).expect("routes");
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
