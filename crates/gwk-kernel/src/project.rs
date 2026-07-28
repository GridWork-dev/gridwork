//! Event → projection rows.
//!
//! [`apply_event`] is the ONLY thing that writes a projection, and it derives
//! every column from the event alone: the payload is the command body that
//! caused it, the row's `version` is the event's `aggregate_version`, and the
//! timestamps are the event's `appended_at`. Nothing reads the clock, nothing
//! reads ambient state, so replaying a log produces byte-identical rows — which
//! is what the checkpoint hash and the scratch rebuild will need.
//!
//! That is also why the live command path routes through here rather than
//! writing its own SQL: one applier means replay cannot disagree with the write
//! that it is replaying.

use gwk_domain::command::KernelCommand;
use gwk_domain::entity::DISPATCH_NODE_INITIAL_STATE;
use gwk_domain::envelope::EventEnvelope;
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

        // The epoch boundary is the log itself — there is no row behind it.
        KernelCommand::ActivateKernel { .. } => {}

        // Named, not caught by a wildcard: the compiler must make the next
        // phase decide about every one of these rather than let it fall into a
        // silent no-op. The submit path refuses them at the boundary, so a log
        // written by THIS kernel can never contain one.
        KernelCommand::SendMessage { .. }
        | KernelCommand::TransitionMessage { .. }
        | KernelCommand::IssueCommand { .. }
        | KernelCommand::TransitionCommand { .. }
        | KernelCommand::RecordCommandOutcome { .. }
        | KernelCommand::OpenGate { .. }
        | KernelCommand::DecideGate { .. }
        | KernelCommand::RecordEvidence { .. }
        | KernelCommand::GrantAuthority { .. }
        | KernelCommand::RevokeAuthority { .. }
        | KernelCommand::RaiseAttention { .. }
        | KernelCommand::ResolveAttention { .. }
        | KernelCommand::IngestRecord { .. } => {
            return Err(Refusal::storage(format!(
                "{} has no projection in this kernel yet",
                command.command_type()
            )));
        }
    }
    Ok(())
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
}
