//! Typed shapes for the payloads that were schema-less JSON in the lineage
//! system this contract supersedes.
//!
//! Nothing here invents behavior: each type freezes a shape that already
//! crosses process boundaries untyped, so the greenfield kernel starts with a
//! contract instead of `unknown`. Tri-state discipline throughout — a field
//! that is ABSENT means "not recorded", `null` is not used, and the generated
//! TypeScript is `field?: T` under `exactOptionalPropertyTypes`.

use crate::ids::{AttemptId, CommandId, CostMicros, EngineId, LeaseId, Seq, TaskId, Timestamp};

/// One live attempt, as pinned inside an orchestrator checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct OpenAttemptRef {
    pub attempt_id: AttemptId,
    pub task_id: TaskId,
    /// The attempt's state AT CHECKPOINT TIME, as its wire string. Recovery
    /// re-reads the live row; this is the crash-time impression, not truth.
    pub state: String,
    pub engine: EngineId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub worktree_lease_id: Option<LeaseId>,
}

/// One held lease, as pinned inside an orchestrator checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct LeaseSnapshot {
    pub lease_id: LeaseId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub holder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub expires_at: Option<Timestamp>,
}

/// One decision waiting on the operator, as pinned inside a checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct PendingApproval {
    /// OPEN bounded string classifying the ask (e.g. `risk_tag`, `design_fork`).
    pub kind: String,
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub subject_ref: Option<String>,
    pub raised_at: Timestamp,
}

/// Spend cursor at checkpoint time (what recovery reconciles against the
/// authoritative cost ledger).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct BudgetCursor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub spent_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub spent_tool_calls: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub spent_cost_micros: Option<CostMicros>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub window_started_at: Option<Timestamp>,
}

/// The content of one orchestrator crash-recovery checkpoint (the snapshot
/// body, not its storage row).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct OrchestratorCheckpoint {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub orchestrator_id: Option<String>,
    /// Monotonic per-orchestrator checkpoint counter.
    pub seq: Seq,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub native_session_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub active_goal: Option<String>,
    /// Opaque reference to the active lifecycle step.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub active_step_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub latest_command_ref: Option<CommandId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub open_attempts: Option<Vec<OpenAttemptRef>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub leases: Option<Vec<LeaseSnapshot>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub pending_approvals: Option<Vec<PendingApproval>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[specta(optional)]
    pub budget_cursor: Option<BudgetCursor>,
}

/// What a review round decided to do about one finding. CLOSED: these are the
/// three buckets [`RoundFindingSummary`] counts, so a fourth action would make
/// its tally stop adding up.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    specta::Type,
)]
#[serde(rename_all = "snake_case")]
pub enum FindingAction {
    AutoFix,
    AskUser,
    NoOp,
}

impl FindingAction {
    /// Every action, in declaration order.
    pub const ALL: &'static [Self] = &[Self::AutoFix, Self::AskUser, Self::NoOp];
}

/// Per-round finding tally appended to an attempt's retry ledger. Actions are
/// the closed finding taxonomy, snake_case ([`FindingAction`]).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct RoundFindingSummary {
    pub total: u32,
    pub auto_fix: u32,
    pub ask_user: u32,
    pub no_op: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkpoint_tri_state_absent_fields_are_omitted() {
        let cp = OrchestratorCheckpoint {
            orchestrator_id: None,
            seq: Seq::new(7),
            native_session_ref: None,
            active_goal: Some("ship the contract".into()),
            active_step_ref: None,
            latest_command_ref: None,
            open_attempts: Some(vec![]),
            leases: None,
            pending_approvals: None,
            budget_cursor: None,
        };
        let json = serde_json::to_string(&cp).expect("serialize");
        // Absent is OMITTED (tri-state), an empty list stays an empty list.
        assert!(!json.contains("orchestrator_id"));
        assert!(!json.contains("leases"));
        assert!(json.contains("\"open_attempts\":[]"));
        assert!(json.contains("\"seq\":\"7\""));
        let back: OrchestratorCheckpoint = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, cp);
    }

    #[test]
    fn finding_actions_are_the_three_buckets_the_summary_counts() {
        let wire: Vec<serde_json::Value> = FindingAction::ALL
            .iter()
            .map(|a| serde_json::to_value(a).expect("serialize"))
            .collect();
        assert_eq!(
            wire,
            [
                serde_json::json!("auto_fix"),
                serde_json::json!("ask_user"),
                serde_json::json!("no_op")
            ]
        );
    }
}
