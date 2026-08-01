//! The `PreToolUse` contract: the ask Claude Code hands the relay on stdin,
//! and the decision the relay must hand back on stdout.
//!
//! `docs/PARITY.md` (this phase's own design contract) further calls
//! `PreToolUse`'s stdout the *only* decision-returning channel available in
//! interactive-TUI mode — the mode [`crate::spawn_tui`] renders — naming an
//! SDK-hosted `canUseTool` callback as the alternative that is unavailable
//! there. Neither `CLAUDE-STREAM-JSON`, `CLAUDE-HEADLESS`, nor
//! `CLAUDE-HOOKS` states that `canUseTool` claim (an earlier draft of this
//! comment cited `CLAUDE-HOOKS` for it, which this crate's own fetch of
//! that exact page had already contradicted — a false citation caught
//! before it shipped). `CLAUDE-AGENT-SDK`, added to the registry after that
//! round, resolves it: `canUseTool` is documented there as "the SDK
//! replacement for the interactive permission prompt," invoked only when
//! permission evaluation "resolves to a prompt" — i.e. it substitutes for
//! the terminal's own interactive prompt for an SDK caller, which is
//! exactly why an external process controlling an interactive-TUI session
//! (no SDK, no `canUseTool` registered) has no channel into that decision
//! except `PreToolUse`'s own stdout.
// Derivation: CLAUDE-HOOKS — `PreToolUse` is "the first enforcement point in
// Claude Code's permission system", and hooks "run in all permission modes
// including... Bypass mode (--bypass-permissions)" — "Step 1 of permission
// evaluation: 1. PreToolUse hook (first decision point)... The hook runs
// regardless of mode, making it the primary enforcement point before any
// other permission check."
// Derivation: CLAUDE-AGENT-SDK — `CanUseTool`: "The function is the SDK
// replacement for the interactive permission prompt: it's invoked only
// when the permission evaluation flow resolves to a prompt."

use gwk_domain::{AttemptId, GateId, KernelCommand};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bound on the rendered Gate question, so a pathological `tool_input`
/// cannot inflate a stored question without limit.
const QUESTION_MAX_BYTES: usize = 4096;

/// The `PreToolUse` hook's stdin payload.
///
/// `deny_unknown_fields` is deliberate here, not merely the security floor's
/// default: this is the approval gate, and a shape this crate does not
/// recognize — a future CLI version's new field — must refuse to parse
/// rather than silently drop context, so the caller fails the ask closed
/// (`deny`) instead of acting on a guess.
// Derivation: CLAUDE-HOOKS — the documented stdin JSON schema: `session_id`,
// `prompt_id` ("absent until first user input"), `transcript_path`, `cwd`,
// `permission_mode`, `effort` (`{"level": ...}`), `hook_event_name`,
// `agent_id`/`agent_type` ("only in subagents"), `tool_name`, `tool_input`,
// `tool_use_id`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreToolUseAsk {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_id: Option<String>,
    pub transcript_path: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Value>,
    pub hook_event_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    pub tool_name: String,
    pub tool_input: Value,
    pub tool_use_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AskDecodeError {
    #[error("malformed PreToolUse ask: {0}")]
    Malformed(String),
    #[error("hook_event_name is {0:?}, not \"PreToolUse\"")]
    WrongEvent(String),
}

impl PreToolUseAsk {
    /// Decode one hook invocation's stdin. Never panics on malformed input.
    pub fn from_stdin(bytes: &[u8]) -> Result<Self, AskDecodeError> {
        let ask: Self =
            serde_json::from_slice(bytes).map_err(|e| AskDecodeError::Malformed(e.to_string()))?;
        if ask.hook_event_name != "PreToolUse" {
            return Err(AskDecodeError::WrongEvent(ask.hook_event_name));
        }
        Ok(ask)
    }

    /// A human-readable rendering of the ask, for the Gate's `question`.
    /// `PreToolUse` carries no natural-language question field on the
    /// wire — this composes one from `tool_name` + `tool_input`, which is
    /// this crate's own rendering choice, not a documented field.
    fn question(&self) -> String {
        let input = serde_json::to_string(&self.tool_input).unwrap_or_default();
        let mut question = format!("{}: {input}", self.tool_name);
        if question.len() > QUESTION_MAX_BYTES {
            question.truncate(QUESTION_MAX_BYTES);
            question.push_str("…(truncated)");
        }
        question
    }
}

/// The relay's answer, and the reason shown to the operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
// Derivation: CLAUDE-HOOKS — `permissionDecision` accepts `allow`, `deny`,
// `ask` (escalate to the interactive permission dialog), or `defer`; a relay
// that always makes an active decision never emits `defer`, so it is not a
// variant here.
pub enum PermissionDecision {
    Allow,
    Deny,
    Ask,
}

impl PermissionDecision {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
            Self::Ask => "ask",
        }
    }
}

/// The three options `PreToolUse`'s decision channel can answer with — what
/// a relayed ask's Gate offers.
pub const GATE_OPTIONS: [&str; 3] = ["allow", "deny", "ask"];

/// The relay's decision, as it travels both over the relay's own wire and
/// into the hook's stdout contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookDecision {
    pub decision: PermissionDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl HookDecision {
    /// The exact stdout JSON `PreToolUse` reads as a decision.
    // Derivation: CLAUDE-HOOKS — "Exit 0 with JSON for structured control":
    // `{"hookSpecificOutput":{"hookEventName":"PreToolUse",
    // "permissionDecision":"allow|deny|ask|defer",
    // "permissionDecisionReason":"..."}}`.
    pub fn to_stdout_json(&self) -> Value {
        let mut inner = serde_json::json!({
            "hookEventName": "PreToolUse",
            "permissionDecision": self.decision.as_str(),
        });
        if let Some(reason) = &self.reason {
            inner["permissionDecisionReason"] = Value::String(reason.clone());
        }
        serde_json::json!({ "hookSpecificOutput": inner })
    }
}

/// Build the kernel's view of a relayed `PreToolUse` ask: a Gate carrying
/// the composed question and the options the relay can answer with.
/// Deciding it stays kernel-side — this only shapes the request.
pub fn open_gate_from_ask(
    gate_id: GateId,
    attempt_id: Option<AttemptId>,
    ask: &PreToolUseAsk,
) -> KernelCommand {
    KernelCommand::OpenGate {
        gate_id,
        attempt_id,
        phase_ref: None,
        kind: Some("approval".to_owned()),
        question: Some(ask.question()),
        options: Some(GATE_OPTIONS.iter().map(|s| (*s).to_owned()).collect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask_json() -> Vec<u8> {
        br#"{
            "session_id": "sess-1",
            "transcript_path": "/tmp/t.json",
            "cwd": "/repo",
            "hook_event_name": "PreToolUse",
            "tool_name": "Bash",
            "tool_input": {"command": "rm -rf /"},
            "tool_use_id": "tool-1"
        }"#
        .to_vec()
    }

    #[test]
    fn a_well_formed_ask_decodes() {
        let ask = PreToolUseAsk::from_stdin(&ask_json()).expect("decode");
        assert_eq!(ask.tool_name, "Bash");
        assert_eq!(ask.tool_use_id, "tool-1");
        assert_eq!(ask.prompt_id, None);
    }

    #[test]
    fn an_unknown_field_is_refused_not_silently_dropped() {
        let mut json: Value = serde_json::from_slice(&ask_json()).expect("valid fixture JSON");
        json["a_future_field_this_crate_does_not_know"] = Value::Bool(true);
        let bytes = serde_json::to_vec(&json).expect("serialize");
        let error = PreToolUseAsk::from_stdin(&bytes).expect_err("unknown field must be refused");
        assert!(matches!(error, AskDecodeError::Malformed(_)));
    }

    #[test]
    fn a_malformed_ask_is_a_typed_error_not_a_panic() {
        let error = PreToolUseAsk::from_stdin(b"not json").expect_err("must be refused");
        assert!(matches!(error, AskDecodeError::Malformed(_)));
    }

    #[test]
    fn the_wrong_hook_event_is_refused() {
        let mut json: Value = serde_json::from_slice(&ask_json()).expect("valid fixture JSON");
        json["hook_event_name"] = Value::String("Stop".to_owned());
        let bytes = serde_json::to_vec(&json).expect("serialize");
        let error = PreToolUseAsk::from_stdin(&bytes).expect_err("wrong event must be refused");
        assert_eq!(error, AskDecodeError::WrongEvent("Stop".to_owned()));
    }

    #[test]
    fn open_gate_offers_exactly_the_three_answerable_options() {
        let ask = PreToolUseAsk::from_stdin(&ask_json()).expect("decode");
        let command =
            open_gate_from_ask(GateId::new("gate-1"), Some(AttemptId::new("att-1")), &ask);
        let KernelCommand::OpenGate {
            question, options, ..
        } = command
        else {
            panic!("expected OpenGate");
        };
        assert_eq!(
            options,
            Some(vec![
                "allow".to_owned(),
                "deny".to_owned(),
                "ask".to_owned()
            ])
        );
        let question = question.expect("a question was composed");
        assert!(question.contains("Bash"));
        assert!(question.contains("rm -rf /"));
    }

    #[test]
    fn an_oversized_tool_input_does_not_grow_the_question_without_bound() {
        let mut json: Value = serde_json::from_slice(&ask_json()).expect("valid fixture JSON");
        json["tool_input"] = Value::String("x".repeat(QUESTION_MAX_BYTES * 4));
        let bytes = serde_json::to_vec(&json).expect("serialize");
        let ask = PreToolUseAsk::from_stdin(&bytes).expect("decode");
        let command = open_gate_from_ask(GateId::new("gate-1"), None, &ask);
        let KernelCommand::OpenGate { question, .. } = command else {
            panic!("expected OpenGate");
        };
        assert!(question.expect("question").len() <= QUESTION_MAX_BYTES + "…(truncated)".len());
    }

    #[test]
    fn a_decision_serializes_to_the_documented_stdout_shape() {
        let decision = HookDecision {
            decision: PermissionDecision::Deny,
            reason: Some("blocked by policy".to_owned()),
        };
        let json = decision.to_stdout_json();
        assert_eq!(json["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(json["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(
            json["hookSpecificOutput"]["permissionDecisionReason"],
            "blocked by policy"
        );
    }

    #[test]
    fn a_decision_with_no_reason_omits_the_reason_key() {
        let decision = HookDecision {
            decision: PermissionDecision::Allow,
            reason: None,
        };
        let json = decision.to_stdout_json();
        assert!(json["hookSpecificOutput"]["permissionDecisionReason"].is_null());
    }

    #[test]
    fn hook_decision_round_trips_through_json() {
        for decision in [
            PermissionDecision::Allow,
            PermissionDecision::Deny,
            PermissionDecision::Ask,
        ] {
            let value = HookDecision {
                decision,
                reason: None,
            };
            let json = serde_json::to_vec(&value).expect("serialize");
            let back: HookDecision = serde_json::from_slice(&json).expect("deserialize");
            assert_eq!(back, value);
        }
    }
}
