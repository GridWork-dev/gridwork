//! One pure check function per `docs/PARITY.md` axis.
//!
//! Every function here takes already-collected, adapter-normalized input —
//! never a raw wire byte, never a live process — and returns a verdict plus
//! a human-readable detail string. `docs/PARITY.md`'s harness section states
//! the load-bearing rule this module exists to satisfy: "a harness axis that
//! cannot fail is a defect." Every check below therefore ships a SEEDED
//! NEGATIVE (a constructed bad input that must fail it) beside a POSITIVE
//! CONTROL (a good input that must pass it) — see this module's `tests`.
//!
//! None of the four is engine-specific: each takes a small, generic shape
//! (tag lists, millisecond timestamps, id pairs, or the kernel's own
//! [`KernelCommand::OpenGate`] value) so the same function judges opencode,
//! Claude, or Codex data alike. The `runners` module is what maps a real,
//! adapter-normalized value (an `Event`, a `Message`, a `CodexEvent`, an
//! `OpenGate` command an adapter function built) into the shape these
//! functions read — that mapping is a re-tagging of already-normalized
//! adapter output, never a re-derivation of protocol behavior.

use std::time::Duration;

use gwk_domain::KernelCommand;

use crate::matrix::Verdict;

/// Axis 1 — lifecycle. Every tag in `expected` must appear in `observed`;
/// `docs/PARITY.md`: "each surfaced as the adapter's normalized state" — a
/// fact that never arrived is exactly what this rejects.
pub fn check_lifecycle_observed(expected: &[&str], observed: &[String]) -> (Verdict, String) {
    let missing: Vec<&str> = expected
        .iter()
        .copied()
        .filter(|e| !observed.iter().any(|o| o == e))
        .collect();
    if missing.is_empty() {
        (
            Verdict::Pass,
            format!("observed every expected lifecycle fact {expected:?}"),
        )
    } else {
        (
            Verdict::Fail,
            format!("missing lifecycle fact(s) {missing:?}; observed {observed:?}"),
        )
    }
}

/// Axis 2 — status truth. `docs/PARITY.md`: "Skip is measured from the
/// engine state change to the adapter's normalized status report" and must
/// land within `bound`. Both timestamps are milliseconds on the same clock
/// — the runner's job, not this function's, is picking what "the state
/// change" and "the observed report" mean for one live interaction.
pub fn check_status_skew(
    state_change_at_ms: u64,
    observed_at_ms: u64,
    bound: Duration,
) -> (Verdict, String) {
    let skew_ms = observed_at_ms.abs_diff(state_change_at_ms);
    let bound_ms = u64::try_from(bound.as_millis()).unwrap_or(u64::MAX);
    if skew_ms <= bound_ms {
        (
            Verdict::Pass,
            format!(
                "status observed {skew_ms}ms after the state change, within the {bound_ms}ms bound"
            ),
        )
    } else {
        (
            Verdict::Fail,
            format!(
                "status observed {skew_ms}ms after the state change, past the {bound_ms}ms bound"
            ),
        )
    }
}

/// Axis 3 — transcript ingestion. `docs/PARITY.md`: "fan-out linkage
/// survives the trip." `children` is `(child_id, parent_id)`; every `Some`
/// parent must resolve inside `known_ids` — a child left pointing at a
/// parent nobody heard of is broken linkage, whether that parent id is
/// merely absent from this collection window or genuinely never existed.
pub fn check_fanout_linkage(
    known_ids: &[String],
    children: &[(String, Option<String>)],
) -> (Verdict, String) {
    let broken: Vec<String> = children
        .iter()
        .filter_map(|(child, parent)| {
            let parent_id = parent.as_ref()?;
            (!known_ids.iter().any(|k| k == parent_id)).then(|| format!("{child} -> {parent_id}"))
        })
        .collect();
    if broken.is_empty() {
        (
            Verdict::Pass,
            format!(
                "{} child link(s) all resolve inside {known_ids:?}",
                children.len()
            ),
        )
    } else {
        (
            Verdict::Fail,
            format!("broken fan-out linkage: {}", broken.join(", ")),
        )
    }
}

/// Axis 4 — approval relay. `docs/PARITY.md`: "Observe `OpenGate` with the
/// engine's question and options" — a gate a human (or a kernel-side policy)
/// cannot actually read is not a relayed prompt, regardless of which engine
/// built it.
pub fn check_gate_carries_question_and_options(command: &KernelCommand) -> (Verdict, String) {
    let KernelCommand::OpenGate {
        question, options, ..
    } = command
    else {
        return (
            Verdict::Fail,
            format!("expected an OpenGate command, got {command:?}"),
        );
    };
    let has_question = question.as_deref().is_some_and(|q| !q.trim().is_empty());
    let has_options = options.as_ref().is_some_and(|o| !o.is_empty());
    match (has_question, has_options) {
        (true, true) => (
            Verdict::Pass,
            format!(
                "gate carries a question ({} bytes) and {} option(s)",
                question.as_deref().unwrap_or_default().len(),
                options.as_ref().map(Vec::len).unwrap_or_default()
            ),
        ),
        (false, true) => (Verdict::Fail, "gate has options but no question".to_owned()),
        (true, false) => (
            Verdict::Fail,
            "gate has a question but no options".to_owned(),
        ),
        (false, false) => (
            Verdict::Fail,
            "gate has neither a question nor options".to_owned(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gwk_adapter_claude::hook::{PreToolUseAsk, open_gate_from_ask};
    use gwk_domain::GateId;

    // ---- axis 1: lifecycle ----

    #[test]
    fn lifecycle_check_passes_when_every_expected_fact_was_observed() {
        let (verdict, _) =
            check_lifecycle_observed(&["start", "end"], &["start".to_owned(), "end".to_owned()]);
        assert_eq!(verdict, Verdict::Pass);
    }

    #[test]
    fn lifecycle_check_fails_on_a_seeded_missing_event() {
        // Positive control above proves it can pass; this proves it can
        // fail — "idle" was never observed.
        let (verdict, detail) = check_lifecycle_observed(
            &["start", "idle", "end"],
            &["start".to_owned(), "end".to_owned()],
        );
        assert_eq!(verdict, Verdict::Fail);
        assert!(
            detail.contains("idle"),
            "detail did not name the miss: {detail}"
        );
    }

    // ---- axis 2: status truth ----

    #[test]
    fn status_skew_check_passes_within_the_bound() {
        let (verdict, _) = check_status_skew(1_000, 1_500, Duration::from_secs(10));
        assert_eq!(verdict, Verdict::Pass);
    }

    #[test]
    fn status_skew_check_fails_on_a_seeded_delay_past_d() {
        let bound = crate::pins::d_bound();
        let bound_ms = u64::try_from(bound.as_millis()).expect("fits");
        let (verdict, detail) = check_status_skew(0, bound_ms + 1, bound);
        assert_eq!(verdict, Verdict::Fail);
        assert!(detail.contains("past"), "detail: {detail}");
    }

    #[test]
    fn status_skew_check_is_symmetric_in_which_timestamp_leads() {
        // A report timestamped BEFORE the state-change timestamp is still a
        // skew (clock jitter between the two sources), never a negative
        // duration this function would panic subtracting.
        let (verdict, _) = check_status_skew(5_000, 4_500, Duration::from_secs(10));
        assert_eq!(verdict, Verdict::Pass);
    }

    // ---- axis 3: transcript ingestion (fan-out linkage) ----

    #[test]
    fn fanout_check_passes_when_every_parent_resolves() {
        let known = vec!["root".to_owned(), "child-a".to_owned()];
        let children = vec![("child-a".to_owned(), Some("root".to_owned()))];
        let (verdict, _) = check_fanout_linkage(&known, &children);
        assert_eq!(verdict, Verdict::Pass);
    }

    #[test]
    fn fanout_check_passes_vacuously_with_no_children() {
        let (verdict, _) = check_fanout_linkage(&[], &[]);
        assert_eq!(verdict, Verdict::Pass);
    }

    #[test]
    fn fanout_check_fails_on_a_seeded_unresolved_parent() {
        let known = vec!["root".to_owned()];
        // "orphan"'s parent "ghost-parent" is not in `known` — exactly the
        // broken-linkage shape docs/PARITY.md's harness section names.
        let children = vec![("orphan".to_owned(), Some("ghost-parent".to_owned()))];
        let (verdict, detail) = check_fanout_linkage(&known, &children);
        assert_eq!(verdict, Verdict::Fail);
        assert!(detail.contains("ghost-parent"), "detail: {detail}");
    }

    // ---- axis 4: approval relay ----

    fn claude_ask() -> PreToolUseAsk {
        // Built through the adapter's own public decoder, never hand-parsed
        // — the same fixture shape gwk-adapter-claude's own hook.rs tests use.
        PreToolUseAsk::from_stdin(
            br#"{
                "session_id": "sess-1",
                "transcript_path": "/tmp/t.json",
                "cwd": "/repo",
                "hook_event_name": "PreToolUse",
                "tool_name": "Bash",
                "tool_input": {"command": "echo hi"},
                "tool_use_id": "tool-1"
            }"#,
        )
        .expect("decode fixture ask")
    }

    #[test]
    fn gate_check_passes_on_a_real_opengate_built_by_the_claude_adapter() {
        let ask = claude_ask();
        let command = open_gate_from_ask(GateId::new("g-1"), None, &ask);
        let (verdict, _) = check_gate_carries_question_and_options(&command);
        assert_eq!(verdict, Verdict::Pass);
    }

    #[test]
    fn gate_check_fails_on_a_seeded_empty_options_list() {
        let command = KernelCommand::OpenGate {
            gate_id: GateId::new("g-1"),
            attempt_id: None,
            phase_ref: None,
            kind: None,
            question: Some("Allow this?".to_owned()),
            options: Some(vec![]),
        };
        let (verdict, detail) = check_gate_carries_question_and_options(&command);
        assert_eq!(verdict, Verdict::Fail);
        assert!(detail.contains("no options"), "detail: {detail}");
    }

    #[test]
    fn gate_check_fails_on_a_seeded_missing_question() {
        let command = KernelCommand::OpenGate {
            gate_id: GateId::new("g-1"),
            attempt_id: None,
            phase_ref: None,
            kind: None,
            question: None,
            options: Some(vec!["allow".to_owned(), "deny".to_owned()]),
        };
        let (verdict, detail) = check_gate_carries_question_and_options(&command);
        assert_eq!(verdict, Verdict::Fail);
        assert!(detail.contains("no question"), "detail: {detail}");
    }

    #[test]
    fn gate_check_fails_on_a_command_that_is_not_an_opengate() {
        let command = KernelCommand::AcquireLease {
            lease_id: gwk_domain::LeaseId::new("l-1"),
            mode: gwk_domain::LeaseMode::Exclusive,
            holder: None,
            scope: None,
            repo: None,
            path: None,
            branch: None,
            base_sha: None,
            expires_at: None,
        };
        let (verdict, detail) = check_gate_carries_question_and_options(&command);
        assert_eq!(verdict, Verdict::Fail);
        assert!(detail.contains("OpenGate"), "detail: {detail}");
    }
}
