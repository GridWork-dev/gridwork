//! Pins the SQL transition seed against the Rust edge tables.
//!
//! The FSM lives in two places — the `EDGES` tables in `src/fsm.rs` and the
//! `gwk.transition` seed in `schema/0001_contract.sql` — with no other drift
//! gate between them. This set-comparison fails on any added, removed, or
//! changed edge on either side.

use std::collections::BTreeSet;

use gwk_domain::fsm::{AttemptState, CommandState, MessageState, StateMachine, TaskState};

/// The generated copy, not `include_str!` of the root file: the source sits
/// outside this package, so `cargo package` leaves it behind and the published
/// crate could not compile this test at all. `xtask -- contract --check` is what
/// holds the constant equal to the file, on every CI run.
use gwk_domain::contract_sql::CONTRACT_SQL as DDL;

type Edge = (String, String, String);

/// A state's serde wire name (the snake_case variant name).
fn wire<S: serde::Serialize + std::fmt::Debug>(state: &S) -> String {
    match serde_json::to_value(state).expect("state serializes") {
        serde_json::Value::String(s) => s,
        other => panic!("state {state:?} serialized to a non-string: {other}"),
    }
}

fn rust_edges() -> BTreeSet<Edge> {
    fn insert<S: StateMachine + serde::Serialize>(set: &mut BTreeSet<Edge>, entity: &str) {
        for (from, to) in S::EDGES {
            set.insert((entity.to_owned(), wire(from), wire(to)));
        }
    }
    let mut set = BTreeSet::new();
    insert::<TaskState>(&mut set, "task");
    insert::<AttemptState>(&mut set, "attempt");
    insert::<MessageState>(&mut set, "message");
    insert::<CommandState>(&mut set, "command");
    set
}

/// The seeded `(entity, from_state, to_state)` rows, parsed out of the single
/// `INSERT INTO gwk.transition` block — anchored on that block, never on other
/// parenthesized lines elsewhere in the file.
fn sql_edges() -> BTreeSet<Edge> {
    let start = DDL
        .find("INSERT INTO gwk.transition")
        .expect("DDL contains the gwk.transition seed block");
    let block = &DDL[start..];
    let block = &block[..block.find(';').expect("seed block is ;-terminated")];
    let mut set = BTreeSet::new();
    for line in block.lines() {
        // A value row quotes exactly three strings: ('entity', 'from', 'to').
        // The INSERT header's unquoted column list yields no quoted parts.
        let quoted: Vec<&str> = line.split('\'').skip(1).step_by(2).collect();
        match quoted.as_slice() {
            [] => {}
            [entity, from, to] => {
                let inserted =
                    set.insert(((*entity).to_owned(), (*from).to_owned(), (*to).to_owned()));
                assert!(inserted, "duplicate seed row: {line:?}");
            }
            other => panic!("unparseable seed row {line:?}: quoted values {other:?}"),
        }
    }
    set
}

#[test]
fn sql_transition_seed_matches_rust_edge_tables() {
    let rust = rust_edges();
    let sql = sql_edges();
    let only_rust: Vec<&Edge> = rust.difference(&sql).collect();
    let only_sql: Vec<&Edge> = sql.difference(&rust).collect();
    assert!(
        only_rust.is_empty() && only_sql.is_empty(),
        "gwk.transition seed and the fsm.rs EDGES tables drifted:\n  \
         in Rust but not in SQL: {only_rust:?}\n  \
         in SQL but not in Rust: {only_sql:?}"
    );
}
