//! Event → projection rows.
//!
//! [`apply_event`] derives every column from the event alone: the payload is
//! the command body that caused it, the row's `version` is the event's
//! `aggregate_version`, and the timestamps are the event's `appended_at`.
//! Nothing reads the clock, nothing reads ambient state, so replaying a log
//! reproduces those rows BYTE for byte — which is what the checkpoint hash and
//! the scratch rebuild rest on. That is also why the live command path routes
//! through here rather than writing its own SQL: one applier means replay
//! cannot disagree with the write that it is replaying.
//!
//! It is the only projection writer for thirteen of the fifteen tables, and the
//! exception is worth stating plainly rather than leaving to be discovered by a
//! rebuild that will not agree. [`write_receipt`] and [`page_attention`] are
//! called by [`crate::submit`] directly, and on the PAGED path they run for a
//! command that is REFUSED — so those rows exist with no event behind them and
//! no replay can produce them. `gwk.receipt` and `gwk.attention_item` are
//! therefore excluded from the checkpoint digest; see
//! [`crate::checkpoint::derived_records`].

use gwk_domain::command::KernelCommand;
use gwk_domain::entity::DISPATCH_NODE_INITIAL_STATE;
use gwk_domain::envelope::EventEnvelope;
use gwk_domain::fsm::GateVerdict;
use gwk_domain::protocol::{KernelErrorCode, KernelResult};
use serde::Deserialize;
use sqlx::{PgConnection, postgres::PgQueryResult};

use crate::numeric::to_numeric_text;

/// A refusal, already shaped as the answer the wire will send.
///
/// The contract makes a refusal a VALUE ([`KernelResult::Error`]), so this
/// carries the wire code from the start instead of being translated at the
/// boundary and losing the reason on the way.
#[derive(Debug, Clone, PartialEq)]
pub struct Refusal {
    pub code: KernelErrorCode,
    pub message: String,
    pub detail: Option<serde_json::Value>,
}

impl Refusal {
    pub fn new(code: KernelErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            detail: None,
        }
    }

    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = Some(detail);
        self
    }

    pub fn validation(message: impl Into<String>) -> Self {
        Self::new(KernelErrorCode::Validation, message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(KernelErrorCode::NotFound, message)
    }

    pub fn storage(message: impl Into<String>) -> Self {
        Self::new(KernelErrorCode::Storage, message)
    }

    pub fn into_result(self) -> KernelResult {
        KernelResult::Error {
            code: self.code,
            message: self.message,
            detail: self.detail,
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl From<gwk_domain::port::AppendError> for Refusal {
    /// The store's refusals, given their wire codes. `VersionConflict` and
    /// `Fenced` carry their numbers into `detail` because a caller that means
    /// to retry needs the actual value, not just the name of the failure.
    fn from(error: gwk_domain::port::AppendError) -> Self {
        use gwk_domain::port::AppendError as E;
        let message = error.to_string();
        match error {
            E::VersionConflict { actual, expected } => {
                Self::new(KernelErrorCode::StaleVersion, message)
                    .with_detail(serde_json::json!({ "actual": actual, "expected": expected }))
            }
            E::Fenced { presented, current } => Self::new(KernelErrorCode::Fenced, message)
                .with_detail(serde_json::json!({
                    "presented": presented.to_string(),
                    "current": current.to_string(),
                })),
            E::MalformedBatch(_) => Self::validation(message),
            E::Storage(_) => Self::storage(message),
        }
    }
}

impl From<gwk_domain::port::StorageError> for Refusal {
    fn from(error: gwk_domain::port::StorageError) -> Self {
        Self::storage(error.0)
    }
}

fn db(context: &str, error: sqlx::Error) -> Refusal {
    // A foreign key that does not resolve is the caller naming something that
    // is not there — an attempt under an uncreated task, a worktree under an
    // uncreated lease. `storage` would read as "the kernel broke" for what is
    // ordinary bad input, so it gets the code that says which.
    if let sqlx::Error::Database(ref db) = error
        && db.is_foreign_key_violation()
    {
        return Refusal::not_found(format!("{context}: {error}"));
    }
    Refusal::storage(format!("{context}: {error}"))
}

/// The wire string of a contract enum, taken from the one serializer the
/// contract already defines — a stored state cannot drift from the JSON one.
pub(crate) fn wire_str<T: serde::Serialize>(value: &T) -> Result<String, Refusal> {
    match serde_json::to_value(value) {
        Ok(serde_json::Value::String(text)) => Ok(text),
        other => Err(Refusal::storage(format!(
            "expected a wire string, got {other:?}"
        ))),
    }
}

/// The inverse: a stored state column back into its contract enum.
pub(crate) fn from_wire_str<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, Refusal> {
    serde_json::from_value(serde_json::Value::String(text.to_owned())).map_err(|e| {
        Refusal::storage(format!(
            "stored state {text:?} is not a state this contract knows: {e}"
        ))
    })
}

/// SQL NULL for an absent optional, never a JSON `null` literal — the wire's
/// tri-state discipline reaches the columns too.
fn json_opt<T: serde::Serialize>(value: Option<&T>) -> Result<Option<serde_json::Value>, Refusal> {
    value
        .map(serde_json::to_value)
        .transpose()
        .map_err(|e| Refusal::storage(format!("serialize projection column: {e}")))
}

/// An update that named a row must have found exactly it.
///
/// Zero rows is `not_found`, not a silent no-op: on the live path it means the
/// caller addressed something that does not exist, and on the replay path it
/// means the log and the projection disagree — both are refusals, never a
/// quietly skipped write.
fn require_one(done: PgQueryResult, kind: &str, id: &str) -> Result<(), Refusal> {
    match done.rows_affected() {
        1 => Ok(()),
        0 => Err(Refusal::not_found(format!("no {kind} {id}"))),
        n => Err(Refusal::storage(format!(
            "{kind} {id} matched {n} rows on a primary key"
        ))),
    }
}

/// Write one event's projection rows into the transaction that appended it.
pub(crate) async fn apply_event(
    conn: &mut PgConnection,
    event: &EventEnvelope,
) -> Result<(), Refusal> {
    // Borrowed, not cloned: an inline payload runs to 64 KiB and this is the
    // hot path.
    let command = KernelCommand::deserialize(&event.payload).map_err(|e| {
        Refusal::storage(format!(
            "event {} payload is not a command body: {e}",
            event.event_id
        ))
    })?;
    // The row's version IS the event's — that equality is what lets a client's
    // `expected_version` mean the same thing to the CAS on the log and to the
    // CAS on the projection row.
    let version = i64::from(event.aggregate_version);
    let at = event.appended_at.as_str();

    match &command {
        // ---- task ----
        KernelCommand::CreateTask {
            task_id,
            kind,
            title,
            spec_ref,
            project,
            priority,
            tracker_ref,
        } => {
            sqlx::query(
                "INSERT INTO gwk.task \
                   (id, version, state, kind, title, spec_ref, project, priority, tracker_ref, \
                    created_at, updated_at) \
                 VALUES ($1, $2, 'submitted', $3, $4, $5, $6, $7, $8, \
                    $9::timestamptz, $9::timestamptz)",
            )
            .bind(task_id.as_str())
            .bind(version)
            .bind(kind.as_deref())
            .bind(title.as_deref())
            .bind(spec_ref.as_deref())
            .bind(project.as_deref())
            .bind(*priority)
            .bind(tracker_ref.as_deref())
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("insert task", e))?;
        }
        KernelCommand::TransitionTask { task_id, to, .. } => {
            let done = sqlx::query(
                "UPDATE gwk.task SET state = $2, version = $3, updated_at = $4::timestamptz \
                 WHERE id = $1",
            )
            .bind(task_id.as_str())
            .bind(wire_str(to)?)
            .bind(version)
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("transition task", e))?;
            require_one(done, "task", task_id.as_str())?;
        }

        // ---- attempt ----
        KernelCommand::CreateAttempt {
            attempt_id,
            task_id,
            engine,
            capability,
            role,
            model_lane,
            permission_profile,
            worktree_lease_id,
            base_sha,
            budget,
        } => {
            sqlx::query(
                "INSERT INTO gwk.attempt \
                   (id, version, state, task_id, engine, capability, role, model_lane, \
                    permission_profile, worktree_lease_id, base_sha, budget, \
                    created_at, updated_at) \
                 VALUES ($1, $2, 'queued', $3, $4, $5, $6, $7, $8, $9, $10, $11, \
                    $12::timestamptz, $12::timestamptz)",
            )
            .bind(attempt_id.as_str())
            .bind(version)
            .bind(task_id.as_str())
            .bind(engine.as_str())
            .bind(capability.as_deref())
            .bind(role.as_deref())
            .bind(model_lane.as_deref())
            .bind(permission_profile.as_deref())
            .bind(worktree_lease_id.as_ref().map(|l| l.as_str()))
            .bind(base_sha.as_deref())
            .bind(json_opt(budget.as_ref())?)
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("insert attempt", e))?;
        }
        KernelCommand::TransitionAttempt { attempt_id, to, .. } => {
            let done = sqlx::query(
                "UPDATE gwk.attempt SET state = $2, version = $3, updated_at = $4::timestamptz \
                 WHERE id = $1",
            )
            .bind(attempt_id.as_str())
            .bind(wire_str(to)?)
            .bind(version)
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("transition attempt", e))?;
            require_one(done, "attempt", attempt_id.as_str())?;
        }
        KernelCommand::UpdateBudget {
            attempt_id, budget, ..
        } => {
            let done = sqlx::query(
                "UPDATE gwk.attempt SET budget = $2, version = $3, updated_at = $4::timestamptz \
                 WHERE id = $1",
            )
            .bind(attempt_id.as_str())
            .bind(json_opt(Some(budget))?)
            .bind(version)
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("update budget", e))?;
            require_one(done, "attempt", attempt_id.as_str())?;
        }
        KernelCommand::RecordAttemptRuntime {
            attempt_id,
            runtime_ref,
            runtime_started_at,
            ..
        } => {
            // No coalesce: the runtime handle is a start-time fact, and a
            // resumed attempt legitimately replaces the handle of the process
            // it no longer is.
            let done = sqlx::query(
                "UPDATE gwk.attempt SET \
                   runtime_ref = $2, \
                   runtime_started_at = $3::timestamptz, \
                   version = $4, updated_at = $5::timestamptz \
                 WHERE id = $1",
            )
            .bind(attempt_id.as_str())
            .bind(runtime_ref)
            .bind(runtime_started_at.as_str())
            .bind(version)
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("record attempt runtime", e))?;
            require_one(done, "attempt", attempt_id.as_str())?;
        }
        KernelCommand::RecordAttemptOutcome {
            attempt_id,
            exit_code,
            provider_terminal_event,
            result_valid,
            evidence_manifest_ref,
            ..
        } => {
            // `coalesce` because absent means "not specified" (the tri-state
            // rule), so a later record that names fewer fields must not erase
            // what an earlier one established.
            let done = sqlx::query(
                "UPDATE gwk.attempt SET \
                   exit_code = coalesce($2, exit_code), \
                   provider_terminal_event = coalesce($3, provider_terminal_event), \
                   result_valid = coalesce($4, result_valid), \
                   evidence_manifest_ref = coalesce($5, evidence_manifest_ref), \
                   version = $6, updated_at = $7::timestamptz \
                 WHERE id = $1",
            )
            .bind(attempt_id.as_str())
            .bind(*exit_code)
            .bind(provider_terminal_event.as_deref())
            .bind(*result_valid)
            .bind(evidence_manifest_ref.as_deref())
            .bind(version)
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("record attempt outcome", e))?;
            require_one(done, "attempt", attempt_id.as_str())?;
        }
        // A round and a finding are facts about the attempt that live in the
        // log; the row carries no ledger for them. It still advances, because
        // its version tracks the aggregate's — letting it lag would put the
        // projection CAS and the log CAS on different numbers.
        KernelCommand::RecordRound { attempt_id, .. }
        | KernelCommand::RecordFinding { attempt_id, .. } => {
            let done = sqlx::query(
                "UPDATE gwk.attempt SET version = $2, updated_at = $3::timestamptz WHERE id = $1",
            )
            .bind(attempt_id.as_str())
            .bind(version)
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("advance attempt", e))?;
            require_one(done, "attempt", attempt_id.as_str())?;
        }

        // ---- engine session ----
        KernelCommand::OpenEngineSession {
            engine_session_id,
            attempt_id,
            engine,
            provider_session_ref,
        } => {
            sqlx::query(
                "INSERT INTO gwk.engine_session \
                   (id, attempt_id, engine, provider_session_ref, started_at) \
                 VALUES ($1, $2, $3, $4, $5::timestamptz)",
            )
            .bind(engine_session_id.as_str())
            .bind(attempt_id.as_str())
            .bind(engine.as_str())
            .bind(provider_session_ref.as_deref())
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("open engine session", e))?;
        }
        KernelCommand::CloseEngineSession { engine_session_id } => {
            let done = sqlx::query(
                "UPDATE gwk.engine_session SET ended_at = $2::timestamptz WHERE id = $1",
            )
            .bind(engine_session_id.as_str())
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("close engine session", e))?;
            require_one(done, "engine_session", engine_session_id.as_str())?;
        }

        // ---- lease ----
        KernelCommand::AcquireLease {
            lease_id,
            mode,
            holder,
            scope,
            repo,
            path,
            branch,
            base_sha,
            expires_at,
        } => {
            sqlx::query(
                "INSERT INTO gwk.lease \
                   (id, version, state, mode, holder, scope, repo, path, branch, base_sha, \
                    expires_at, created_at, updated_at) \
                 VALUES ($1, $2, 'held', $3, $4, $5, $6, $7, $8, $9, $10::timestamptz, \
                    $11::timestamptz, $11::timestamptz)",
            )
            .bind(lease_id.as_str())
            .bind(version)
            .bind(wire_str(mode)?)
            .bind(holder.as_deref())
            .bind(scope.as_deref())
            .bind(repo.as_deref())
            .bind(path.as_deref())
            .bind(branch.as_deref())
            .bind(base_sha.as_deref())
            .bind(expires_at.as_ref().map(|t| t.as_str()))
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("acquire lease", e))?;
        }
        KernelCommand::RenewLease {
            lease_id,
            expires_at,
            ..
        } => {
            let done = sqlx::query(
                "UPDATE gwk.lease SET \
                   expires_at = coalesce($2::timestamptz, expires_at), \
                   heartbeat_at = $3::timestamptz, version = $4, updated_at = $3::timestamptz \
                 WHERE id = $1",
            )
            .bind(lease_id.as_str())
            .bind(expires_at.as_ref().map(|t| t.as_str()))
            .bind(at)
            .bind(version)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("renew lease", e))?;
            require_one(done, "lease", lease_id.as_str())?;
        }
        KernelCommand::ReleaseLease {
            lease_id,
            disposition,
            ..
        } => {
            let done = sqlx::query(
                "UPDATE gwk.lease SET state = 'released', \
                   disposition = coalesce($2, disposition), \
                   version = $3, updated_at = $4::timestamptz \
                 WHERE id = $1",
            )
            .bind(lease_id.as_str())
            .bind(disposition.as_deref())
            .bind(version)
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("release lease", e))?;
            require_one(done, "lease", lease_id.as_str())?;
        }
        KernelCommand::ExpireLease { lease_id, .. } => {
            let done = sqlx::query(
                "UPDATE gwk.lease SET state = 'expired', version = $2, \
                   updated_at = $3::timestamptz \
                 WHERE id = $1",
            )
            .bind(lease_id.as_str())
            .bind(version)
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("expire lease", e))?;
            require_one(done, "lease", lease_id.as_str())?;
        }

        // ---- worktree ----
        KernelCommand::RegisterWorktree {
            worktree_id,
            repo,
            path,
            branch,
            base_sha,
            lease_id,
        } => {
            sqlx::query(
                "INSERT INTO gwk.worktree (id, repo, path, branch, base_sha, lease_id, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7::timestamptz)",
            )
            .bind(worktree_id.as_str())
            .bind(repo)
            .bind(path)
            .bind(branch)
            .bind(base_sha.as_deref())
            .bind(lease_id.as_ref().map(|l| l.as_str()))
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("register worktree", e))?;
        }
        KernelCommand::UpdateWorktree {
            worktree_id,
            dirty,
            unpushed,
            base_sha,
        } => {
            let done = sqlx::query(
                "UPDATE gwk.worktree SET dirty = $2, unpushed = $3, \
                   base_sha = coalesce($4, base_sha) \
                 WHERE id = $1",
            )
            .bind(worktree_id.as_str())
            .bind(*dirty)
            .bind(*unpushed)
            .bind(base_sha.as_deref())
            .execute(&mut *conn)
            .await
            .map_err(|e| db("update worktree", e))?;
            require_one(done, "worktree", worktree_id.as_str())?;
        }
        KernelCommand::ReleaseWorktree {
            worktree_id,
            disposition,
        } => {
            let done = sqlx::query(
                "UPDATE gwk.worktree SET released_at = $2::timestamptz, \
                   disposition = coalesce($3, disposition) \
                 WHERE id = $1",
            )
            .bind(worktree_id.as_str())
            .bind(at)
            .bind(disposition.as_deref())
            .execute(&mut *conn)
            .await
            .map_err(|e| db("release worktree", e))?;
            require_one(done, "worktree", worktree_id.as_str())?;
        }

        // ---- dispatch tree ----
        KernelCommand::RegisterDispatchNode {
            dispatch_node_id,
            parent_id,
            attempt_id,
            kind,
            label,
        } => {
            sqlx::query(
                "INSERT INTO gwk.dispatch_node \
                   (id, version, parent_id, attempt_id, kind, state, label, \
                    created_at, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8::timestamptz, $8::timestamptz)",
            )
            .bind(dispatch_node_id.as_str())
            .bind(version)
            .bind(parent_id.as_ref().map(|p| p.as_str()))
            .bind(attempt_id.as_ref().map(|a| a.as_str()))
            .bind(kind)
            .bind(DISPATCH_NODE_INITIAL_STATE)
            .bind(label.as_deref())
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("register dispatch node", e))?;
        }
        KernelCommand::TransitionDispatchNode {
            dispatch_node_id,
            to,
            ..
        } => {
            let done = sqlx::query(
                "UPDATE gwk.dispatch_node SET state = $2, version = $3, \
                   updated_at = $4::timestamptz \
                 WHERE id = $1",
            )
            .bind(dispatch_node_id.as_str())
            .bind(to)
            .bind(version)
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("transition dispatch node", e))?;
            require_one(done, "dispatch_node", dispatch_node_id.as_str())?;
        }

        // ---- orchestrator ----
        KernelCommand::WriteOrchestratorCheckpoint { checkpoint } => {
            // Latest-per-orchestrator, so this is an upsert rather than a
            // ledger. The seq guard rides the UPDATE branch: a rewind — the one
            // way recovery could resume from a superseded snapshot — is refused
            // by the trigger even if this path ever stopped checking first.
            let orchestrator_id = checkpoint.orchestrator_id.as_deref().ok_or_else(|| {
                Refusal::validation("a checkpoint without an orchestrator_id has no identity")
            })?;
            sqlx::query(
                "INSERT INTO gwk.orchestrator_checkpoint \
                   (orchestrator_id, seq, native_session_ref, active_goal, active_step_ref, \
                    latest_command_ref, open_attempts, leases, pending_approvals, budget_cursor, \
                    updated_at) \
                 VALUES ($1, $2::numeric, $3, $4, $5, $6, $7, $8, $9, $10, $11::timestamptz) \
                 ON CONFLICT (orchestrator_id) DO UPDATE SET \
                   seq = EXCLUDED.seq, \
                   native_session_ref = EXCLUDED.native_session_ref, \
                   active_goal = EXCLUDED.active_goal, \
                   active_step_ref = EXCLUDED.active_step_ref, \
                   latest_command_ref = EXCLUDED.latest_command_ref, \
                   open_attempts = EXCLUDED.open_attempts, \
                   leases = EXCLUDED.leases, \
                   pending_approvals = EXCLUDED.pending_approvals, \
                   budget_cursor = EXCLUDED.budget_cursor, \
                   updated_at = EXCLUDED.updated_at",
            )
            .bind(orchestrator_id)
            .bind(to_numeric_text(checkpoint.seq.value()))
            .bind(checkpoint.native_session_ref.as_deref())
            .bind(checkpoint.active_goal.as_deref())
            .bind(checkpoint.active_step_ref.as_deref())
            .bind(checkpoint.latest_command_ref.as_ref().map(|c| c.as_str()))
            .bind(json_opt(checkpoint.open_attempts.as_ref())?)
            .bind(json_opt(checkpoint.leases.as_ref())?)
            .bind(json_opt(checkpoint.pending_approvals.as_ref())?)
            .bind(json_opt(checkpoint.budget_cursor.as_ref())?)
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("write orchestrator checkpoint", e))?;
        }

        // ---- message ----
        KernelCommand::SendMessage {
            message_id,
            correlation_id,
            reply_to,
            sender,
            recipient,
            channel,
            kind,
            payload,
            deadline,
        } => {
            // `gwk.message.idempotency_key` is NOT NULL, and the message body
            // carries none — the key that names this send is the ENVELOPE's,
            // which the event preserves. Taking it from there rather than
            // minting one keeps the row's uniqueness the same uniqueness the
            // log already enforced, instead of a second, weaker one.
            let key = event.idempotency_key.as_ref().ok_or_else(|| {
                Refusal::validation("a message needs the idempotency key that sent it")
            })?;
            sqlx::query(
                "INSERT INTO gwk.message \
                   (id, version, state, idempotency_key, correlation_id, reply_to, sender, \
                    recipient, channel, kind, payload, deadline, created_at, updated_at) \
                 VALUES ($1, $2, 'accepted', $3, $4, $5, $6, $7, $8, $9, $10, \
                    $11::timestamptz, $12::timestamptz, $12::timestamptz)",
            )
            .bind(message_id.as_str())
            .bind(version)
            .bind(key.as_str())
            .bind(correlation_id.as_ref().map(|c| c.as_str()))
            .bind(reply_to.as_ref().map(|m| m.as_str()))
            .bind(sender.as_deref())
            .bind(recipient.as_deref())
            .bind(channel.as_deref())
            .bind(kind.as_deref())
            .bind(json_opt(payload.as_ref())?)
            .bind(deadline.as_ref().map(|d| d.as_str()))
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("insert message", e))?;
        }
        KernelCommand::TransitionMessage {
            message_id,
            to,
            dead_letter_reason,
            ..
        } => {
            // COALESCE, not a plain assignment: a reason is written once, by
            // the transition that carries it, and a later edge with no reason
            // of its own must not erase why the message died.
            let done = sqlx::query(
                "UPDATE gwk.message \
                 SET state = $2, version = $3, updated_at = $4::timestamptz, \
                     dead_letter_reason = COALESCE($5, dead_letter_reason) \
                 WHERE id = $1",
            )
            .bind(message_id.as_str())
            .bind(wire_str(to)?)
            .bind(version)
            .bind(at)
            .bind(dead_letter_reason.as_deref())
            .execute(&mut *conn)
            .await
            .map_err(|e| db("transition message", e))?;
            require_one(done, "message", message_id.as_str())?;
        }

        // ---- execution command ----
        KernelCommand::IssueCommand {
            command_id,
            kind,
            targets,
            actor,
        } => {
            sqlx::query(
                "INSERT INTO gwk.command \
                   (id, version, state, kind, target, actor, idempotency_key, \
                    created_at, updated_at) \
                 VALUES ($1, $2, 'issued', $3, $4, $5, $6, $7::timestamptz, $7::timestamptz)",
            )
            .bind(command_id.as_str())
            .bind(version)
            .bind(kind)
            // The contract says the projection keeps the FIRST target as its
            // primary; the full list stays in the event, which is where a
            // multi-target stop is reconstructed from.
            .bind(targets.first())
            .bind(json_opt(actor.as_ref())?)
            .bind(event.idempotency_key.as_ref().map(|k| k.as_str()))
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("insert command", e))?;
        }
        KernelCommand::TransitionCommand { command_id, to, .. } => {
            let done = sqlx::query(
                "UPDATE gwk.command SET state = $2, version = $3, updated_at = $4::timestamptz \
                 WHERE id = $1",
            )
            .bind(command_id.as_str())
            .bind(wire_str(to)?)
            .bind(version)
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("transition command", e))?;
            require_one(done, "command", command_id.as_str())?;
        }
        KernelCommand::RecordCommandOutcome {
            command_id,
            outcome,
            ..
        } => {
            // ONE statement writing both, because `outcome_iff_verification_complete`
            // makes any other order unrepresentable: the outcome column may
            // hold a value only while the state is `verification_complete`, and
            // that state may exist only with one. So this command IS the
            // terminal transition, not a follow-up to it — which is also why
            // `TransitionCommand` refuses to name that state itself.
            let done = sqlx::query(
                "UPDATE gwk.command \
                 SET state = 'verification_complete', outcome = $2, version = $3, \
                     updated_at = $4::timestamptz \
                 WHERE id = $1",
            )
            .bind(command_id.as_str())
            .bind(wire_str(outcome)?)
            .bind(version)
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("record command outcome", e))?;
            require_one(done, "command", command_id.as_str())?;
        }

        // ---- gate ----
        KernelCommand::OpenGate {
            gate_id,
            attempt_id,
            phase_ref,
            kind,
            question,
            options,
        } => {
            sqlx::query(
                "INSERT INTO gwk.gate \
                   (id, version, attempt_id, phase_ref, kind, question, options, verdict, \
                    created_at, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, 'pending', $8::timestamptz, \
                         $8::timestamptz)",
            )
            .bind(gate_id.as_str())
            .bind(version)
            .bind(attempt_id.as_ref().map(|a| a.as_str()))
            .bind(phase_ref.as_deref())
            .bind(kind.as_deref())
            .bind(question.as_deref())
            .bind(json_opt(options.as_ref())?)
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("insert gate", e))?;
        }
        KernelCommand::DecideGate {
            gate_id,
            verdict,
            chosen_option,
            evidence_ref,
            ..
        } => {
            let decided_by = (!matches!(verdict, GateVerdict::Pending)).then_some(&event.actor);
            // `chosen_option` replaces rather than coalesces, exactly like the
            // verdict beside it: a re-decide is a whole new decision, and a
            // stale choice under a fresh verdict would misreport what was
            // answered back to the engine.
            let done = sqlx::query(
                "UPDATE gwk.gate \
                 SET verdict = $2, chosen_option = $3, decided_by = $4, version = $5, \
                     updated_at = $6::timestamptz, \
                     evidence_ref = COALESCE($7, evidence_ref) \
                 WHERE id = $1",
            )
            .bind(gate_id.as_str())
            .bind(wire_str(verdict)?)
            .bind(chosen_option.as_deref())
            .bind(json_opt(decided_by)?)
            .bind(version)
            .bind(at)
            .bind(evidence_ref.as_deref())
            .execute(&mut *conn)
            .await
            .map_err(|e| db("decide gate", e))?;
            require_one(done, "gate", gate_id.as_str())?;
        }

        // ---- evidence ----
        KernelCommand::RecordEvidence {
            evidence_id,
            kind,
            r#ref,
            digest,
            byte_size,
        } => {
            // No version column and no updated_at: a piece of evidence is a
            // fact about something that already happened, so the row is
            // written once and never moves. Its event still advances the
            // aggregate version — the log's count is not the row's business.
            sqlx::query(
                "INSERT INTO gwk.evidence (id, kind, ref, digest, byte_size, created_at) \
                 VALUES ($1, $2, $3, $4, $5::numeric, $6::timestamptz)",
            )
            .bind(evidence_id.as_str())
            .bind(kind)
            .bind(r#ref)
            .bind(digest.as_deref())
            .bind(byte_size.map(|b| to_numeric_text(b.value())))
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("insert evidence", e))?;
        }

        // ---- cost ----
        KernelCommand::RecordCostEntry {
            cost_entry_id,
            attempt_id,
            engine_session_id,
            dispatch_node_id,
            engine,
            model,
            input_tokens,
            cached_input_tokens,
            cache_write_tokens,
            output_tokens,
            reasoning_tokens,
            cost_micros,
            cost_is_estimate,
        } => {
            // Ledger row like evidence: written once, never moves. Totals are
            // a rollup query, so no accumulator column advances here.
            sqlx::query(
                "INSERT INTO gwk.cost_entry \
                   (id, attempt_id, engine_session_id, dispatch_node_id, engine, model, \
                    input_tokens, cached_input_tokens, cache_write_tokens, output_tokens, \
                    reasoning_tokens, cost_micros, cost_is_estimate, recorded_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7::numeric, $8::numeric, $9::numeric, \
                         $10::numeric, $11::numeric, $12::numeric, $13, $14::timestamptz)",
            )
            .bind(cost_entry_id.as_str())
            .bind(attempt_id.as_ref().map(|id| id.as_str()))
            .bind(engine_session_id.as_ref().map(|id| id.as_str()))
            .bind(dispatch_node_id.as_ref().map(|id| id.as_str()))
            .bind(engine.as_str())
            .bind(model.as_deref())
            .bind(input_tokens.map(|t| to_numeric_text(t.value())))
            .bind(cached_input_tokens.map(|t| to_numeric_text(t.value())))
            .bind(cache_write_tokens.map(|t| to_numeric_text(t.value())))
            .bind(output_tokens.map(|t| to_numeric_text(t.value())))
            .bind(reasoning_tokens.map(|t| to_numeric_text(t.value())))
            .bind(cost_micros.map(|c| to_numeric_text(c.value())))
            .bind(*cost_is_estimate)
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("insert cost entry", e))?;
        }

        // ---- authority ----
        KernelCommand::GrantAuthority {
            authority_grant_id,
            grantee,
            action_class,
            scope,
            expires_at,
        } => {
            sqlx::query(
                "INSERT INTO gwk.authority_grant \
                   (id, grantee, action_class, scope, granted_at, expires_at) \
                 VALUES ($1, $2, $3, $4, $5::timestamptz, $6::timestamptz)",
            )
            .bind(authority_grant_id.as_str())
            .bind(json_opt(Some(grantee))?)
            .bind(action_class)
            .bind(scope.as_deref())
            .bind(at)
            .bind(expires_at.as_ref().map(|t| t.as_str()))
            .execute(&mut *conn)
            .await
            .map_err(|e| db("insert authority grant", e))?;
        }
        KernelCommand::RevokeAuthority {
            authority_grant_id,
            reason,
        } => {
            // `revoked_at` is set from the EVENT, not `now()`: replay must
            // reach the same grant table it did live, and a grant's liveness is
            // read against a command's `issued_at`.
            let done = sqlx::query(
                "UPDATE gwk.authority_grant \
                 SET revoked_at = $2::timestamptz, revoke_reason = COALESCE($3, revoke_reason) \
                 WHERE id = $1",
            )
            .bind(authority_grant_id.as_str())
            .bind(at)
            .bind(reason.as_deref())
            .execute(&mut *conn)
            .await
            .map_err(|e| db("revoke authority grant", e))?;
            require_one(done, "authority_grant", authority_grant_id.as_str())?;
        }

        // ---- attention ----
        KernelCommand::RaiseAttention {
            attention_item_id,
            kind,
            summary,
            subject_ref,
            raised_by,
            priority,
        } => {
            sqlx::query(
                "INSERT INTO gwk.attention_item \
                   (id, kind, summary, subject_ref, raised_by, priority, raised_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7::timestamptz)",
            )
            .bind(attention_item_id.as_str())
            .bind(kind)
            .bind(summary)
            .bind(subject_ref.as_deref())
            .bind(json_opt(raised_by.as_ref())?)
            .bind(*priority)
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("insert attention item", e))?;
        }
        KernelCommand::ResolveAttention {
            attention_item_id,
            resolution,
        } => {
            // Only an UNRESOLVED item resolves. Resolving twice would move the
            // stamp and, worse, silently re-open the (kind, subject_ref) dedup
            // slot's history — `require_one` turns that into a not-found.
            let done = sqlx::query(
                "UPDATE gwk.attention_item \
                 SET resolved_at = $2::timestamptz, resolution = COALESCE($3, resolution) \
                 WHERE id = $1 AND resolved_at IS NULL",
            )
            .bind(attention_item_id.as_str())
            .bind(at)
            .bind(resolution.as_deref())
            .execute(&mut *conn)
            .await
            .map_err(|e| db("resolve attention item", e))?;
            require_one(
                done,
                "unresolved attention_item",
                attention_item_id.as_str(),
            )?;
        }
        // All three stamps predicate on the id ALONE — deliberately not on
        // `resolved_at IS NULL` the way resolve does. A stamp is
        // last-write-wins, so the only thing that can go wrong is naming an
        // item that does not exist, and `require_one` turns that into the
        // typed not-found. Adding an open-ness predicate here would make the
        // second press of an idempotent key a spurious error.
        //
        // None of them writes `resolved_at`. That is the whole mechanism behind
        // "mute is never a resolve": the row keeps its (kind, subject_ref)
        // dedup slot, so silencing a problem does not re-arm it — and unmuting
        // it does not re-arm it either, because there was nothing to re-arm.
        KernelCommand::AckAttention { attention_item_id } => {
            let done = sqlx::query(
                "UPDATE gwk.attention_item SET acked_at = $2::timestamptz WHERE id = $1",
            )
            .bind(attention_item_id.as_str())
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("ack attention item", e))?;
            require_one(done, "attention_item", attention_item_id.as_str())?;
        }
        KernelCommand::MuteAttention {
            attention_item_id,
            muted_until,
        } => {
            // From the COMMAND, not `now()` plus a duration: replay has to
            // rebuild the same deadline it built live.
            let done = sqlx::query(
                "UPDATE gwk.attention_item SET muted_until = $2::timestamptz WHERE id = $1",
            )
            .bind(attention_item_id.as_str())
            .bind(muted_until.as_str())
            .execute(&mut *conn)
            .await
            .map_err(|e| db("mute attention item", e))?;
            require_one(done, "attention_item", attention_item_id.as_str())?;
        }
        KernelCommand::UnmuteAttention { attention_item_id } => {
            // Back to NULL rather than to a past timestamp: absent is what the
            // column already means by "audible", so a cleared mute and a
            // never-muted item read identically instead of leaving a stale
            // deadline every reader has to compare against the clock.
            let done =
                sqlx::query("UPDATE gwk.attention_item SET muted_until = NULL WHERE id = $1")
                    .bind(attention_item_id.as_str())
                    .execute(&mut *conn)
                    .await
                    .map_err(|e| db("unmute attention item", e))?;
            require_one(done, "attention_item", attention_item_id.as_str())?;
        }

        // ---- workspace tree ----
        KernelCommand::CreateWorkspaceNode {
            workspace_node_id,
            kind,
            parent_id,
            session_id,
        } => {
            sqlx::query(
                "INSERT INTO gwk.workspace_node \
                   (id, version, kind, parent_id, session_id, created_at, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6::timestamptz, $6::timestamptz)",
            )
            .bind(workspace_node_id.as_str())
            .bind(version)
            .bind(kind.as_str())
            .bind(parent_id.as_ref().map(|p| p.as_str()))
            .bind(session_id.as_ref().map(|s| s.as_str()))
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("create workspace node", e))?;
        }
        KernelCommand::MoveWorkspaceNode {
            workspace_node_id,
            parent_id,
            ..
        } => {
            let done = sqlx::query(
                "UPDATE gwk.workspace_node SET parent_id = $2, version = $3, \
                   updated_at = $4::timestamptz \
                 WHERE id = $1",
            )
            .bind(workspace_node_id.as_str())
            .bind(parent_id.as_str())
            .bind(version)
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("move workspace node", e))?;
            require_one(done, "workspace_node", workspace_node_id.as_str())?;
        }
        KernelCommand::RebindWorkspacePane {
            workspace_node_id,
            session_id,
            ..
        } => {
            let done = sqlx::query(
                "UPDATE gwk.workspace_node SET session_id = $2, version = $3, \
                   updated_at = $4::timestamptz \
                 WHERE id = $1",
            )
            .bind(workspace_node_id.as_str())
            .bind(session_id.as_str())
            .bind(version)
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("rebind workspace pane", e))?;
            require_one(done, "workspace_node", workspace_node_id.as_str())?;
        }
        KernelCommand::CloseWorkspaceNode {
            workspace_node_id, ..
        } => {
            // A real DELETE: the wire entity has no closed state, so a closed
            // node leaves the tree. The log keeps the history, and a replay of
            // any legal history deletes children before their parent — submit
            // refuses a close while children are live.
            let done = sqlx::query("DELETE FROM gwk.workspace_node WHERE id = $1")
                .bind(workspace_node_id.as_str())
                .execute(&mut *conn)
                .await
                .map_err(|e| db("close workspace node", e))?;
            require_one(done, "workspace_node", workspace_node_id.as_str())?;
        }

        // ---- workflow runs ----
        KernelCommand::OpenWorkflowRun {
            workflow_run_id,
            template_ref,
            template_sha256,
            task_id,
            title,
        } => {
            sqlx::query(
                "INSERT INTO gwk.workflow_run \
                   (id, version, state, template_ref, template_sha256, task_id, title, \
                    opened_at, updated_at) \
                 VALUES ($1, $2, 'running', $3, $4, $5, $6, $7::timestamptz, $7::timestamptz)",
            )
            .bind(workflow_run_id.as_str())
            .bind(version)
            .bind(template_ref)
            .bind(template_sha256.as_deref())
            .bind(task_id.as_ref().map(|t| t.as_str()))
            .bind(title.as_deref())
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("open workflow run", e))?;
        }
        KernelCommand::AdvanceWorkflowRun {
            workflow_run_id,
            step,
            ..
        } => {
            let done = sqlx::query(
                "UPDATE gwk.workflow_run SET step = $2, version = $3, \
                   updated_at = $4::timestamptz \
                 WHERE id = $1",
            )
            .bind(workflow_run_id.as_str())
            .bind(step)
            .bind(version)
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("advance workflow run", e))?;
            require_one(done, "workflow_run", workflow_run_id.as_str())?;
        }
        KernelCommand::CloseWorkflowRun {
            workflow_run_id,
            outcome,
            ..
        } => {
            // No DELETE here: a run is ledger history, and the no-delete
            // trigger holds the row in place. Close stamps the terminal state.
            let done = sqlx::query(
                "UPDATE gwk.workflow_run SET state = $2, version = $3, \
                   closed_at = $4::timestamptz, updated_at = $4::timestamptz \
                 WHERE id = $1",
            )
            .bind(workflow_run_id.as_str())
            .bind(outcome)
            .bind(version)
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("close workflow run", e))?;
            require_one(done, "workflow_run", workflow_run_id.as_str())?;
        }

        // ---- pty sessions ----
        KernelCommand::OpenPtySession {
            pty_session_id,
            generation,
            engine_session_id,
            title,
        } => {
            sqlx::query(
                "INSERT INTO gwk.pty_session \
                   (id, version, state, generation, engine_session_id, attach_count, detach_count, title, \
                     opened_at, updated_at) \
                  VALUES ($1, $2, 'running', $3, $4, 0, 0, $5, $6::timestamptz, $6::timestamptz)",
            )
            .bind(pty_session_id.as_str())
            .bind(version)
            .bind(generation.as_str())
            .bind(engine_session_id.as_ref().map(|id| id.as_str()))
            .bind(title.as_deref())
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("open pty session", e))?;
        }
        KernelCommand::RecordPtyAttach { pty_session_id, .. } => {
            let done = sqlx::query(
                "UPDATE gwk.pty_session SET attach_count = attach_count + 1, \
                   version = $2, updated_at = $3::timestamptz \
                 WHERE id = $1",
            )
            .bind(pty_session_id.as_str())
            .bind(version)
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("record pty attach", e))?;
            require_one(done, "pty_session", pty_session_id.as_str())?;
        }
        KernelCommand::RecordPtyDetach { pty_session_id, .. } => {
            let done = sqlx::query(
                "UPDATE gwk.pty_session SET detach_count = detach_count + 1, \
                   version = $2, updated_at = $3::timestamptz \
                 WHERE id = $1",
            )
            .bind(pty_session_id.as_str())
            .bind(version)
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("record pty detach", e))?;
            require_one(done, "pty_session", pty_session_id.as_str())?;
        }
        KernelCommand::SendPtyInput {
            pty_session_id,
            generation,
            ..
        } => {
            let lifetime = gwk_domain::ids::pty_session_lifetime_id(pty_session_id, generation);
            let done = sqlx::query(
                "UPDATE gwk.pty_session SET version = $2, updated_at = $3::timestamptz \
                 WHERE id = $1",
            )
            .bind(lifetime.as_str())
            .bind(version)
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("record pty input", e))?;
            require_one(done, "pty_session", lifetime.as_str())?;
        }
        KernelCommand::ClosePtySession { pty_session_id, .. } => {
            // No DELETE: the S8 receipt reads closed sessions' rows, and the
            // no-delete trigger holds them in place. Close stamps the state.
            let done = sqlx::query(
                "UPDATE gwk.pty_session SET state = 'closed', version = $2, \
                   closed_at = $3::timestamptz, updated_at = $3::timestamptz \
                 WHERE id = $1",
            )
            .bind(pty_session_id.as_str())
            .bind(version)
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("close pty session", e))?;
            require_one(done, "pty_session", pty_session_id.as_str())?;
        }

        // The epoch boundary is the log itself — there is no row behind it.
        KernelCommand::ActivateKernel { .. } => {}

        // ---- ingestion ----
        KernelCommand::IngestRecord {
            kind,
            payload,
            payload_ref,
        } => {
            // The id is the event's aggregate id, which `route_of` derived from
            // the envelope's project and idempotency key. Re-deriving it here
            // would need the envelope, which a replay does not have — and the
            // event already carries the answer, which is the rule this whole
            // module follows: every column comes off the EVENT.
            sqlx::query(
                "INSERT INTO gwk.ingested_record \
                   (id, kind, payload, payload_ref, ingested_by, event_seq, ingested_at) \
                 VALUES ($1, $2, $3, $4, $5, $6::numeric, $7::timestamptz)",
            )
            .bind(event.aggregate_id.as_str())
            .bind(kind.as_str())
            .bind(payload.clone())
            .bind(json_opt(payload_ref.as_ref())?)
            .bind(json_opt(Some(&event.actor))?)
            .bind(to_numeric_text(event.global_sequence.value()))
            .bind(at)
            .execute(&mut *conn)
            .await
            .map_err(|e| db("insert ingested record", e))?;
        }
    }
    Ok(())
}

/// Write the immutable attestation every authority result owes.
///
/// `ON CONFLICT DO NOTHING` because a receipt is a fact, not a counter: a
/// denied or paged command writes no event, so the idempotency pre-check —
/// which reads the log — cannot see the retry coming, and re-attesting the
/// identical fact must be a no-op rather than a primary-key exception.
pub(crate) async fn write_receipt(
    conn: &mut PgConnection,
    receipt: &gwk_domain::entity::Receipt,
) -> Result<(), Refusal> {
    let actor = json_opt(Some(&receipt.actor))?;
    let inserted = sqlx::query(
        "INSERT INTO gwk.receipt \
           (id, actor, action, subject_type, subject_id, from_state, to_state, \
            observed_basis, ts) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9::timestamptz) \
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(receipt.id.as_str())
    .bind(actor.clone())
    .bind(&receipt.action)
    .bind(&receipt.subject_type)
    .bind(&receipt.subject_id)
    .bind(receipt.from.as_deref())
    .bind(receipt.to.as_deref())
    .bind(receipt.observed_basis.as_deref())
    .bind(receipt.ts.as_str())
    .execute(&mut *conn)
    .await
    .map_err(|e| db("write receipt", e))?;
    if inserted.rows_affected() == 1 {
        return Ok(());
    }

    // A receipt is immutable, so an identical retry is a no-op and a changed
    // fact under the same id is an idempotency conflict. Silently keeping the
    // old row would let a refused send later succeed while its only receipt
    // continued to attest the refusal.
    let identical: bool = sqlx::query_scalar(
        "SELECT actor = $2::jsonb \
             AND action = $3 \
             AND subject_type = $4 \
             AND subject_id = $5 \
             AND from_state IS NOT DISTINCT FROM $6 \
             AND to_state IS NOT DISTINCT FROM $7 \
             AND observed_basis IS NOT DISTINCT FROM $8 \
             AND ts = $9::timestamptz \
           FROM gwk.receipt WHERE id = $1",
    )
    .bind(receipt.id.as_str())
    .bind(actor)
    .bind(&receipt.action)
    .bind(&receipt.subject_type)
    .bind(&receipt.subject_id)
    .bind(receipt.from.as_deref())
    .bind(receipt.to.as_deref())
    .bind(receipt.observed_basis.as_deref())
    .bind(receipt.ts.as_str())
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| db("compare existing receipt", e))?;
    if identical {
        Ok(())
    } else {
        Err(Refusal::new(
            KernelErrorCode::IdempotencyConflict,
            format!(
                "receipt {} already records a different authority decision",
                receipt.id
            ),
        ))
    }
}

/// Raise the deduplicated attention item a page owes.
///
/// `ON CONFLICT DO NOTHING` against the unresolved-dedup index: a page fires
/// once per real problem, so the second command to hit the same
/// `(kind, subject_ref)` joins the open item instead of stacking a new one.
/// Deliberately different from the explicit `RaiseAttention` command, which
/// refuses a duplicate rather than silently doing nothing — there the caller
/// named an id and expects a row under it.
pub(crate) async fn page_attention(
    conn: &mut PgConnection,
    attention_item_id: &str,
    summary: &str,
    subject_ref: &str,
    raised_by: &serde_json::Value,
    at: &str,
) -> Result<(), Refusal> {
    // One kind, fixed here rather than passed: every item this path raises is
    // the same thing — the kernel refused a gated command. The caller-facing
    // `RaiseAttention` is where an open kind belongs.
    //
    // `priority` is left unset for the same reason: this path has no opinion
    // about rank, and NULL already means unranked. Omission here, not oversight.
    let kind = "authority";
    sqlx::query(
        "INSERT INTO gwk.attention_item \
           (id, kind, summary, subject_ref, raised_by, raised_at) \
         VALUES ($1, $2, $3, $4, $5, $6::timestamptz) \
         ON CONFLICT (kind, subject_ref) WHERE resolved_at IS NULL DO NOTHING",
    )
    .bind(attention_item_id)
    .bind(kind)
    .bind(summary)
    .bind(subject_ref)
    .bind(raised_by)
    .bind(at)
    .execute(&mut *conn)
    .await
    .map_err(|e| db("page attention", e))?;
    Ok(())
}

/// Is an unresolved item already covering this `(kind, subject_ref)`?
///
/// The pre-check in front of the explicit `RaiseAttention` command, so a
/// duplicate comes back as a typed refusal naming the open item rather than as
/// a unique-violation raised from inside the projection.
pub(crate) async fn unresolved_attention(
    conn: &mut PgConnection,
    kind: &str,
    subject_ref: Option<&str>,
) -> Result<Option<String>, Refusal> {
    let Some(subject_ref) = subject_ref else {
        // NULL subject_ref is distinct in the dedup index, so there is nothing
        // to collide with and nothing to pre-check.
        return Ok(None);
    };
    sqlx::query_scalar(
        "SELECT id FROM gwk.attention_item \
         WHERE kind = $1 AND subject_ref = $2 AND resolved_at IS NULL",
    )
    .bind(kind)
    .bind(subject_ref)
    .fetch_optional(conn)
    .await
    .map_err(|e| db("read unresolved attention", e))
}

#[cfg(test)]
mod tests {
    use gwk_domain::fsm::{AttemptState, LeaseMode, TaskState};

    use super::*;

    #[test]
    fn wire_strings_round_trip_through_the_contract_serializer() {
        // The stored `state` column and the JSON state must be the same string;
        // deriving both from serde is what makes that true by construction
        // rather than by a hand-kept table.
        assert_eq!(
            wire_str(&TaskState::InputRequired).as_deref(),
            Ok("input_required")
        );
        assert_eq!(wire_str(&AttemptState::Blocked).as_deref(), Ok("blocked"));
        assert_eq!(wire_str(&LeaseMode::Exclusive).as_deref(), Ok("exclusive"));
        assert_eq!(
            from_wire_str::<AttemptState>("blocked"),
            Ok(AttemptState::Blocked)
        );
        // A state this contract does not know is a storage refusal, never a
        // best-effort guess at the nearest one.
        assert!(from_wire_str::<TaskState>("half_done").is_err());
    }

    #[test]
    fn an_absent_optional_becomes_sql_null_not_a_json_null() {
        let absent: Option<&gwk_domain::entity::Budget> = None;
        assert_eq!(json_opt(absent), Ok(None));
        let present = gwk_domain::entity::Budget {
            max_tokens: Some(5),
            max_tool_calls: None,
            max_wall_ms: None,
            max_cost_micros: None,
        };
        assert_eq!(
            json_opt(Some(&present)),
            Ok(Some(serde_json::json!({ "max_tokens": 5 })))
        );
    }

    #[test]
    fn the_closed_ingestion_set_is_the_same_one_in_the_ddl_and_the_contract() {
        // `gwk.ingested_record.kind` is the one CLOSED classification column in
        // the schema, and the reason is that the absence of an import path
        // has to be a property of the DATABASE, not a convention the writing
        // process happens to honor. That only holds while the CHECK and
        // `IngestionKind` name the same twelve values, and nothing else here
        // compares them.
        let ddl = crate::contract_sql::CONTRACT_SQL;
        let check = ddl
            .split_once("CREATE TABLE gwk.ingested_record")
            .and_then(|(_, rest)| rest.split_once("kind IN ("))
            .and_then(|(_, rest)| rest.split_once("))"))
            .map(|(list, _)| list)
            .expect("the ingested_record kind CHECK");
        let listed: Vec<&str> = check
            .split(',')
            .map(|value| value.trim().trim_matches('\''))
            .collect();
        let contract: Vec<&str> = gwk_domain::ingestion::IngestionKind::ALL
            .iter()
            .map(|kind| kind.as_str())
            .collect();
        assert_eq!(listed, contract);

        // The door that closes, checked against the DDL rather than only
        // against the enum: a CHECK that admits it would let the row in even
        // though the contract type cannot name it.
        for forbidden in ["import", "migrate", "backfill", "legacy"] {
            assert!(!listed.contains(&forbidden), "the DDL admits {forbidden}");
        }
    }
}
