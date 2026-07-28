//! The four public state machines, as data.
//!
//! Each FSM is an enum plus a fixed `EDGES` table — the table IS the contract.
//! Terminality is derived (a state with no outgoing edge is terminal), and the
//! invariant suite in `tests/` walks these tables exhaustively, so an edit here
//! is a contract change by construction, never a silent drift.
//!
//! Wire values are the snake_case variant names, exactly.

/// A state machine whose full shape is compile-time data.
pub trait StateMachine: Copy + Eq + std::fmt::Debug + Sized + 'static {
    /// Every state, in declaration order.
    const STATES: &'static [Self];
    /// Every legal `(from, to)` edge.
    const EDGES: &'static [(Self, Self)];
    /// The escape set: states an executor may enter WITHOUT fabricating
    /// progress (cancellation / failure / unknown paths). The reachability
    /// invariant requires every non-terminal to reach a terminal using only
    /// edges INTO this set. Empty = plain totality (the command FSM, where a
    /// sweep legitimately drives forward).
    const ESCAPE: &'static [Self];

    /// Terminal = no outgoing edges. Derived from [`Self::EDGES`], so the
    /// table cannot disagree with it.
    fn is_terminal(self) -> bool {
        !Self::EDGES.iter().any(|(from, _)| *from == self)
    }

    fn can_transition(from: Self, to: Self) -> bool {
        Self::EDGES.contains(&(from, to))
    }
}

macro_rules! edges {
    ($ty:ident: $($from:ident -> $to:ident),+ $(,)?) => {
        &[$(($ty::$from, $ty::$to)),+]
    };
}

/// Tracker-visible work item lifecycle (A2A-semantically-verbatim, snake-normalized).
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
pub enum TaskState {
    Submitted,
    Working,
    InputRequired,
    Completed,
    Failed,
    Canceled,
}

impl StateMachine for TaskState {
    const STATES: &'static [Self] = &[
        Self::Submitted,
        Self::Working,
        Self::InputRequired,
        Self::Completed,
        Self::Failed,
        Self::Canceled,
    ];
    const EDGES: &'static [(Self, Self)] = edges![TaskState:
        Submitted -> Working,
        Submitted -> Canceled,
        Working -> InputRequired,
        Working -> Completed,
        Working -> Failed,
        Working -> Canceled,
        InputRequired -> Working,
        InputRequired -> Canceled,
    ];
    const ESCAPE: &'static [Self] = &[Self::Canceled, Self::Failed];
}

/// One engine execution of a task. `unknown` is a first-class terminal — an
/// attempt whose real outcome cannot be determined is never mapped to `failed`.
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
pub enum AttemptState {
    Queued,
    Leased,
    Starting,
    Running,
    Blocked,
    Canceling,
    Canceled,
    Failed,
    Unknown,
    Succeeded,
}

impl StateMachine for AttemptState {
    const STATES: &'static [Self] = &[
        Self::Queued,
        Self::Leased,
        Self::Starting,
        Self::Running,
        Self::Blocked,
        Self::Canceling,
        Self::Canceled,
        Self::Failed,
        Self::Unknown,
        Self::Succeeded,
    ];
    const EDGES: &'static [(Self, Self)] = edges![AttemptState:
        Queued -> Leased,
        Queued -> Canceled,
        Leased -> Starting,
        Leased -> Canceled,
        Starting -> Running,
        Starting -> Failed,
        Starting -> Unknown,
        Starting -> Canceling,
        // running <-> blocked is a receipted CAS flip with exactly ONE legal
        // writer, the liveness producer (enforced in `transition::apply`).
        Running -> Blocked,
        Running -> Canceling,
        Running -> Failed,
        Running -> Unknown,
        Running -> Succeeded,
        Blocked -> Running,
        Blocked -> Canceling,
        Blocked -> Failed,
        Blocked -> Unknown,
        Canceling -> Canceled,
        Canceling -> Unknown,
    ];
    const ESCAPE: &'static [Self] = &[Self::Canceling, Self::Canceled, Self::Failed, Self::Unknown];
}

/// Governed inter-party message delivery.
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
pub enum MessageState {
    Accepted,
    Delivered,
    Acknowledged,
    Applied,
    Rejected,
    DeadLetter,
}

impl StateMachine for MessageState {
    const STATES: &'static [Self] = &[
        Self::Accepted,
        Self::Delivered,
        Self::Acknowledged,
        Self::Applied,
        Self::Rejected,
        Self::DeadLetter,
    ];
    const EDGES: &'static [(Self, Self)] = edges![MessageState:
        Accepted -> Delivered,
        Accepted -> DeadLetter,
        Delivered -> Acknowledged,
        Delivered -> DeadLetter,
        Acknowledged -> Applied,
        Acknowledged -> Rejected,
        Acknowledged -> DeadLetter,
    ];
    const ESCAPE: &'static [Self] = &[Self::DeadLetter];
}

/// A stop/kill command's progress spine. The terminal claims only that
/// verification RAN — the result lives in [`Outcome`], written atomically with
/// the terminal transition.
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
pub enum CommandState {
    Issued,
    Targeted,
    Signaled,
    VerificationComplete,
}

impl StateMachine for CommandState {
    const STATES: &'static [Self] = &[
        Self::Issued,
        Self::Targeted,
        Self::Signaled,
        Self::VerificationComplete,
    ];
    const EDGES: &'static [(Self, Self)] = edges![CommandState:
        Issued -> Targeted,
        Targeted -> Signaled,
        Signaled -> VerificationComplete,
    ];
    // Plain totality: a stop sweep legitimately drives the spine forward, so
    // there is no separate escape set (seam-10).
    const ESCAPE: &'static [Self] = &[];
}

/// A command's verified result — a plain column, NOT an FSM state. Present iff
/// the command reached `verification_complete`; written in the same transaction.
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
pub enum Outcome {
    Clean,
    Partial,
    Unknown,
}

/// A gate's verdict — a CLOSED set (the gate `kind` is open).
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
pub enum GateVerdict {
    Pending,
    Pass,
    Fail,
    Partial,
}

/// A lease's status set (not one of the four public FSMs — expiry is
/// time-driven, release is holder-driven).
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
pub enum LeaseState {
    Held,
    Released,
    Expired,
}

/// Lease sharing mode.
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
pub enum LeaseMode {
    Exclusive,
    Shared,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminality_is_derived_from_edges() {
        assert!(TaskState::Completed.is_terminal());
        assert!(!TaskState::Submitted.is_terminal());
        assert!(AttemptState::Unknown.is_terminal());
        assert!(!AttemptState::Canceling.is_terminal());
        assert!(MessageState::DeadLetter.is_terminal());
        assert!(CommandState::VerificationComplete.is_terminal());
    }

    #[test]
    fn wire_values_are_snake_case() {
        assert_eq!(
            serde_json::to_value(TaskState::InputRequired).expect("serialize"),
            serde_json::json!("input_required")
        );
        assert_eq!(
            serde_json::to_value(CommandState::VerificationComplete).expect("serialize"),
            serde_json::json!("verification_complete")
        );
        assert_eq!(
            serde_json::to_value(MessageState::DeadLetter).expect("serialize"),
            serde_json::json!("dead_letter")
        );
    }
}
