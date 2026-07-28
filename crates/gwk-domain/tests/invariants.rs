//! The FSM invariant suite: exhaustive walks over the fixed edge tables, the
//! carry pins that freeze the contract shape, and mutation fixtures proving
//! the checkers actually detect violations.
//!
//! No property-testing framework: each graph has ≤ 10 states, so exhaustive
//! BFS over the real tables is total, deterministic, and dependency-free.

use gwk_domain::envelope::Actor;
use gwk_domain::fsm::{AttemptState, CommandState, MessageState, StateMachine, TaskState};
use gwk_domain::ids::{IdempotencyKey, ReceiptId};
use gwk_domain::transition::{
    Cursor, LIVENESS_PRODUCER_KIND, TransitionRequest, TransitionResult, apply,
};

// ============================================================
// Checkers — parameterized by the edge table so the mutation
// fixtures below can feed them corrupted graphs.
// ============================================================

/// Terminal = no outgoing edge in `edges`.
fn terminals<S: Copy + Eq>(states: &[S], edges: &[(S, S)]) -> Vec<S> {
    states
        .iter()
        .copied()
        .filter(|s| !edges.iter().any(|(from, _)| from == s))
        .collect()
}

/// BFS from `start` over `edges`, restricted to edges whose TARGET is allowed.
fn reaches_terminal<S: Copy + Eq>(
    start: S,
    states: &[S],
    edges: &[(S, S)],
    allowed_target: impl Fn(S) -> bool,
) -> bool {
    let terminal = terminals(states, edges);
    let mut queue = vec![start];
    let mut seen = vec![start];
    while let Some(s) = queue.pop() {
        if terminal.contains(&s) {
            return true;
        }
        for (from, to) in edges {
            if *from == s && allowed_target(*to) && !seen.contains(to) {
                seen.push(*to);
                queue.push(*to);
            }
        }
    }
    false
}

/// Every non-terminal reaches a terminal using only edges INTO the escape set
/// (progress edges forbidden — cancellation must never fabricate progress).
/// An empty escape set means plain totality: any edge is allowed.
fn escape_reachability<S: Copy + Eq + std::fmt::Debug>(
    states: &[S],
    edges: &[(S, S)],
    escape: &[S],
) -> Result<(), Vec<S>> {
    let terminal = terminals(states, edges);
    let stranded: Vec<S> = states
        .iter()
        .copied()
        .filter(|s| !terminal.contains(s))
        .filter(|s| {
            !reaches_terminal(*s, states, edges, |to| {
                escape.is_empty() || escape.contains(&to)
            })
        })
        .collect();
    if stranded.is_empty() {
        Ok(())
    } else {
        Err(stranded)
    }
}

/// No terminal has an outgoing edge (immutability at the graph level).
fn terminal_immutability<S: Copy + Eq + std::fmt::Debug>(
    expected_terminals: &[S],
    edges: &[(S, S)],
) -> Result<(), Vec<S>> {
    let leaky: Vec<S> = expected_terminals
        .iter()
        .copied()
        .filter(|t| edges.iter().any(|(from, _)| from == t))
        .collect();
    if leaky.is_empty() { Ok(()) } else { Err(leaky) }
}

fn assert_machine<S: StateMachine>(expected_terminals: &[S]) {
    escape_reachability(S::STATES, S::EDGES, S::ESCAPE)
        .unwrap_or_else(|s| panic!("states stranded off the escape paths: {s:?}"));
    // Plain totality holds for every machine, escape set or not.
    escape_reachability(S::STATES, S::EDGES, &[])
        .unwrap_or_else(|s| panic!("states with no path to any terminal: {s:?}"));
    terminal_immutability(expected_terminals, S::EDGES)
        .unwrap_or_else(|t| panic!("terminals with outgoing edges: {t:?}"));
    assert_eq!(
        terminals(S::STATES, S::EDGES),
        expected_terminals,
        "derived terminal set drifted"
    );
}

// ============================================================
// The invariant walks over the real tables
// ============================================================

#[test]
fn task_reaches_terminals_via_escape_edges_only() {
    assert_machine::<TaskState>(&[TaskState::Completed, TaskState::Failed, TaskState::Canceled]);
}

#[test]
fn attempt_reaches_terminals_via_escape_edges_only() {
    assert_machine::<AttemptState>(&[
        AttemptState::Canceled,
        AttemptState::Failed,
        AttemptState::Unknown,
        AttemptState::Succeeded,
    ]);
}

#[test]
fn message_reaches_terminals_via_escape_edges_only() {
    assert_machine::<MessageState>(&[
        MessageState::Applied,
        MessageState::Rejected,
        MessageState::DeadLetter,
    ]);
}

#[test]
fn command_is_plainly_total() {
    // The command spine has no escape set: a stop sweep legitimately drives
    // forward (seam-10). ESCAPE = [] means the escape walk IS plain totality.
    assert!(CommandState::ESCAPE.is_empty());
    assert_machine::<CommandState>(&[CommandState::VerificationComplete]);
}

// ============================================================
// Carry pins — the exact shape, frozen
// ============================================================

#[test]
fn state_counts_are_pinned() {
    assert_eq!(TaskState::STATES.len(), 6);
    assert_eq!(AttemptState::STATES.len(), 10);
    assert_eq!(MessageState::STATES.len(), 6);
    assert_eq!(CommandState::STATES.len(), 4);
}

#[test]
fn edge_counts_are_pinned() {
    assert_eq!(TaskState::EDGES.len(), 8);
    assert_eq!(AttemptState::EDGES.len(), 19);
    assert_eq!(MessageState::EDGES.len(), 7);
    assert_eq!(CommandState::EDGES.len(), 3);
}

#[test]
fn every_wire_value_is_the_snake_case_name() {
    fn wire<S: serde::Serialize>(s: &S) -> String {
        match serde_json::to_value(s).expect("serialize") {
            serde_json::Value::String(v) => v,
            other => panic!("state serialized as non-string: {other}"),
        }
    }
    let task: Vec<String> = TaskState::STATES.iter().map(wire).collect();
    assert_eq!(
        task,
        [
            "submitted",
            "working",
            "input_required",
            "completed",
            "failed",
            "canceled"
        ]
    );
    let attempt: Vec<String> = AttemptState::STATES.iter().map(wire).collect();
    assert_eq!(
        attempt,
        [
            "queued",
            "leased",
            "starting",
            "running",
            "blocked",
            "canceling",
            "canceled",
            "failed",
            "unknown",
            "succeeded"
        ]
    );
    let message: Vec<String> = MessageState::STATES.iter().map(wire).collect();
    assert_eq!(
        message,
        [
            "accepted",
            "delivered",
            "acknowledged",
            "applied",
            "rejected",
            "dead_letter"
        ]
    );
    let command: Vec<String> = CommandState::STATES.iter().map(wire).collect();
    assert_eq!(
        command,
        ["issued", "targeted", "signaled", "verification_complete"]
    );
}

/// The lineage system's edge tables, in their ORIGINAL spelling, mapped through
/// the naming freeze. Every old edge must survive; the additions must be
/// EXACTLY the documented new escape edges — nothing else changed.
#[test]
fn old_edges_carry_under_the_mapping_and_additions_are_exact() {
    fn old_task(name: &str) -> TaskState {
        match name {
            "submitted" => TaskState::Submitted,
            "working" => TaskState::Working,
            "input-required" => TaskState::InputRequired,
            "completed" => TaskState::Completed,
            "failed" => TaskState::Failed,
            "canceled" => TaskState::Canceled,
            other => panic!("unmapped old task state {other}"),
        }
    }
    fn old_attempt(name: &str) -> AttemptState {
        match name {
            "queued" => AttemptState::Queued,
            "leased" => AttemptState::Leased,
            "starting" => AttemptState::Starting,
            "running" => AttemptState::Running,
            "blocked" => AttemptState::Blocked,
            "cancelling" => AttemptState::Canceling,
            "cancelled" => AttemptState::Canceled,
            "failed" => AttemptState::Failed,
            "unknown" => AttemptState::Unknown,
            "succeeded" => AttemptState::Succeeded,
            other => panic!("unmapped old attempt state {other}"),
        }
    }
    fn old_message(name: &str) -> MessageState {
        match name {
            "accepted" => MessageState::Accepted,
            "delivered" => MessageState::Delivered,
            "acknowledged" => MessageState::Acknowledged,
            "applied" => MessageState::Applied,
            "rejected" => MessageState::Rejected,
            "dead-letter" => MessageState::DeadLetter,
            other => panic!("unmapped old message state {other}"),
        }
    }
    fn old_command(name: &str) -> CommandState {
        match name {
            "issued" => CommandState::Issued,
            "targeted" => CommandState::Targeted,
            "signaled" => CommandState::Signaled,
            "verified-stopped" => CommandState::VerificationComplete,
            other => panic!("unmapped old command state {other}"),
        }
    }

    // Verbatim from the lineage schema's transition table.
    let old_task_edges = [
        ("submitted", "working"),
        ("working", "input-required"),
        ("input-required", "working"),
        ("working", "completed"),
        ("working", "failed"),
        ("working", "canceled"),
    ];
    let old_attempt_edges = [
        ("queued", "leased"),
        ("leased", "starting"),
        ("starting", "running"),
        ("running", "blocked"),
        ("blocked", "running"),
        ("running", "cancelling"),
        ("running", "failed"),
        ("running", "unknown"),
        ("running", "succeeded"),
        ("blocked", "cancelling"),
        ("blocked", "failed"),
        ("blocked", "unknown"),
        ("cancelling", "cancelled"),
        ("cancelling", "unknown"),
    ];
    let old_message_edges = [
        ("accepted", "delivered"),
        ("delivered", "acknowledged"),
        ("acknowledged", "applied"),
        ("acknowledged", "rejected"),
        ("accepted", "dead-letter"),
        ("delivered", "dead-letter"),
        ("acknowledged", "dead-letter"),
    ];
    let old_command_edges = [
        ("issued", "targeted"),
        ("targeted", "signaled"),
        ("signaled", "verified-stopped"),
    ];

    fn check_carry<S: Copy + Eq + std::fmt::Debug + Ord>(
        old: &[(&str, &str)],
        map: impl Fn(&str) -> S,
        new_edges: &[(S, S)],
        expected_new: &[(S, S)],
    ) {
        let mapped: Vec<(S, S)> = old.iter().map(|(f, t)| (map(f), map(t))).collect();
        for edge in &mapped {
            assert!(new_edges.contains(edge), "old edge lost: {edge:?}");
        }
        let mut additions: Vec<(S, S)> = new_edges
            .iter()
            .copied()
            .filter(|e| !mapped.contains(e))
            .collect();
        let mut expected = expected_new.to_vec();
        additions.sort();
        expected.sort();
        assert_eq!(additions, expected, "undocumented edge change");
    }

    check_carry(
        &old_task_edges,
        old_task,
        TaskState::EDGES,
        // Cancellation totality: cancel became legal from every pre-terminal
        // waiting state, not only `working`.
        &[
            (TaskState::Submitted, TaskState::Canceled),
            (TaskState::InputRequired, TaskState::Canceled),
        ],
    );
    check_carry(
        &old_attempt_edges,
        old_attempt,
        AttemptState::EDGES,
        // The pre-running escape edges: queued/leased can cancel, starting can
        // fail, vanish, or begin canceling without ever fabricating a run.
        &[
            (AttemptState::Queued, AttemptState::Canceled),
            (AttemptState::Leased, AttemptState::Canceled),
            (AttemptState::Starting, AttemptState::Failed),
            (AttemptState::Starting, AttemptState::Unknown),
            (AttemptState::Starting, AttemptState::Canceling),
        ],
    );
    check_carry(&old_message_edges, old_message, MessageState::EDGES, &[]);
    check_carry(&old_command_edges, old_command, CommandState::EDGES, &[]);
}

// ============================================================
// Blocked round-trip + CAS through apply()
// ============================================================

#[test]
fn blocked_round_trip_is_a_receipted_cas_walk() {
    let liveness = Actor {
        kind: LIVENESS_PRODUCER_KIND.into(),
        id: Some("lp-1".into()),
    };
    let receipt = ReceiptId::new("r-1");
    let mut cursor = Cursor {
        state: AttemptState::Running,
        version: 10,
        applied_idempotency_key: None,
        applied_by: None,
    };
    // running -> blocked -> running -> blocked: every hop receipted, +1 each.
    for (hop, to) in [
        AttemptState::Blocked,
        AttemptState::Running,
        AttemptState::Blocked,
    ]
    .into_iter()
    .enumerate()
    {
        let result = apply(
            &cursor,
            &TransitionRequest {
                to,
                expected_version: cursor.version,
                actor: &liveness,
                idempotency_key: None,
                receipt_id: Some(&receipt),
            },
        );
        match result {
            TransitionResult::Applied { state, version } => {
                assert_eq!(state, to);
                assert_eq!(version, 10 + hop as u32 + 1);
                cursor.state = state;
                cursor.version = version;
            }
            other => panic!("hop {hop} refused: {other:?}"),
        }
    }
    // From blocked, every terminal escape path is open to ordinary actors.
    let kernel = Actor {
        kind: "kernel".into(),
        id: None,
    };
    for to in [
        AttemptState::Canceling,
        AttemptState::Failed,
        AttemptState::Unknown,
    ] {
        let result = apply(
            &cursor,
            &TransitionRequest {
                to,
                expected_version: cursor.version,
                actor: &kernel,
                idempotency_key: None,
                receipt_id: None,
            },
        );
        assert!(
            matches!(result, TransitionResult::Applied { .. }),
            "blocked -> {to:?} must stay open"
        );
    }
}

#[test]
fn terminals_refuse_every_transition_through_apply() {
    let actor = Actor {
        kind: "kernel".into(),
        id: None,
    };
    for terminal in [
        AttemptState::Canceled,
        AttemptState::Failed,
        AttemptState::Unknown,
        AttemptState::Succeeded,
    ] {
        for to in AttemptState::STATES {
            let result = apply(
                &Cursor {
                    state: terminal,
                    version: 1,
                    applied_idempotency_key: None,
                    applied_by: None,
                },
                &TransitionRequest {
                    to: *to,
                    expected_version: 1,
                    actor: &actor,
                    idempotency_key: None,
                    receipt_id: None,
                },
            );
            assert!(
                matches!(result, TransitionResult::IllegalEdge { .. }),
                "{terminal:?} -> {to:?} must be illegal"
            );
        }
    }
}

// ============================================================
// Mutation fixtures — corrupted inputs MUST fail the checkers
// ============================================================

#[test]
fn mutant_flipped_edge_is_caught() {
    // Flip working -> canceled into canceled -> working: the canceled terminal
    // grows an outgoing edge. The immutability checker must refuse it.
    let mut edges: Vec<(TaskState, TaskState)> = TaskState::EDGES.to_vec();
    let idx = edges
        .iter()
        .position(|e| *e == (TaskState::Working, TaskState::Canceled))
        .expect("edge exists");
    edges[idx] = (TaskState::Canceled, TaskState::Working);
    assert!(
        terminal_immutability(
            &[TaskState::Completed, TaskState::Failed, TaskState::Canceled],
            &edges
        )
        .is_err(),
        "the checker accepted a terminal with an outgoing edge"
    );
}

#[test]
fn mutant_missing_escape_edge_is_caught() {
    // Drop queued -> canceled: queued's only remaining walk runs through
    // leased (a progress edge), so escape reachability must refuse it.
    let edges: Vec<(AttemptState, AttemptState)> = AttemptState::EDGES
        .iter()
        .copied()
        .filter(|e| *e != (AttemptState::Queued, AttemptState::Canceled))
        .collect();
    let stranded = escape_reachability(AttemptState::STATES, &edges, AttemptState::ESCAPE)
        .expect_err("the checker accepted a state with no escape path");
    assert_eq!(stranded, vec![AttemptState::Queued]);
}

#[test]
fn mutant_version_skip_is_caught() {
    // A writer that lost a CAS race and blindly retries with a bumped
    // expectation (version + 1) must get a refusal, never an apply.
    let actor = Actor {
        kind: "kernel".into(),
        id: None,
    };
    let cursor = Cursor {
        state: TaskState::Submitted,
        version: 3,
        applied_idempotency_key: None,
        applied_by: None,
    };
    let result = apply(
        &cursor,
        &TransitionRequest {
            to: TaskState::Working,
            expected_version: 4,
            actor: &actor,
            idempotency_key: None,
            receipt_id: None,
        },
    );
    assert_eq!(
        result,
        TransitionResult::StaleVersion {
            actual: 3,
            expected: 4
        }
    );
}

#[test]
fn mutant_wrong_actor_flip_is_caught() {
    // An engine impersonating the liveness producer's EDGE (not its identity)
    // must be refused even with a receipt in hand.
    let engine = Actor {
        kind: "engine".into(),
        id: Some("e-1".into()),
    };
    let receipt = ReceiptId::new("r-forged");
    let result = apply(
        &Cursor {
            state: AttemptState::Running,
            version: 5,
            applied_idempotency_key: None,
            applied_by: None,
        },
        &TransitionRequest {
            to: AttemptState::Blocked,
            expected_version: 5,
            actor: &engine,
            idempotency_key: None,
            receipt_id: Some(&receipt),
        },
    );
    assert!(matches!(result, TransitionResult::UnauthorizedActor { .. }));
}

#[test]
fn mutant_duplicate_idempotency_key_does_not_double_apply() {
    let actor = Actor {
        kind: "kernel".into(),
        id: None,
    };
    let key = IdempotencyKey::new("submit-once");
    let cursor = Cursor {
        state: TaskState::Working,
        version: 2,
        applied_idempotency_key: Some(key.clone()),
        // The retry below is the SAME actor that recorded the applied key, so
        // the idempotent echo still fires — a keyed replay is bound to its
        // recording actor.
        applied_by: Some(actor.clone()),
    };
    let result = apply(
        &cursor,
        &TransitionRequest {
            to: TaskState::Working,
            expected_version: 1,
            actor: &actor,
            idempotency_key: Some(&key),
            receipt_id: None,
        },
    );
    // Stable replay: the version does NOT advance a second time.
    assert_eq!(
        result,
        TransitionResult::Applied {
            state: TaskState::Working,
            version: 2
        }
    );
}
