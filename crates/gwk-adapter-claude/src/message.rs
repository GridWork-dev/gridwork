//! Typed parsing of the bidirectional `stream-json` message stream, and
//! subagent-tree reconstruction from `parent_tool_use_id` chaining.
//!
//! What is modeled here is deliberately narrow. `docs/derivation/SPECS.md`'s
//! `CLAUDE-HEADLESS` row scopes a citation to what
//! `code.claude.com/docs/en/headless` actually says, and that page documents
//! stream-json's *behavior* far more than every message type's full field
//! layout. `CLAUDE-AGENT-SDK` fills part of that gap: the TypeScript SDK
//! reference publishes `SDKResultMessage`'s actual field-level type, which —
//! being the typed surface over the same wire messages, not a separate
//! protocol — is this module's basis for the `result`-message fields below.
//! One field stays unresolved even so: a `tool_use` block's own JSON shape.
//! Neither permitted page defines it (`CLAUDE-AGENT-SDK` explicitly punts to
//! `MessageParam`, "From Anthropic SDK" — a third, uncited surface), so
//! [`SubagentTree`] treats it as an escalation. Fields this module cannot
//! cite at all stay reachable through [`Message::raw`] rather than being
//! asserted as typed, spec-derived facts.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

/// One line of the `stream-json` output stream, decoded far enough to route
/// it. Untyped fields stay in `raw`; nothing here asserts a shape it cannot
/// cite.
// Derivation: CLAUDE-HEADLESS — a message's `type` discriminates it
// (`"system"`, `"assistant"`, `"user"`, `"result"` are all named on the
// page), and `subtype` is the same message's own sub-discriminant — shown
// concretely on `system/api_retry` (`subtype: "api_retry"`) and
// `system/plugin_install` (`subtype: "plugin_install"`), both of which also
// carry `session_id`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Message {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub subtype: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    // Derivation: CLAUDE-HEADLESS — "Messages from subagents appear in the
    // stream as `assistant` and `user` messages whose `parent_tool_use_id`
    // field is the ID of the tool call that spawned the subagent. Messages
    // from the main conversation carry `null` in that field." Nesting is
    // arbitrary depth: "when a subagent spawns its own subagent, the nested
    // subagent's messages carry the ID of the Agent tool call that spawned
    // it in `parent_tool_use_id`, so you can rebuild the full nesting tree
    // by following those IDs." Subagent TEXT/thinking blocks additionally
    // require `--forward-subagent-text` (`control_command` does not pass it
    // — that is a caller-side choice, not this parser's).
    #[serde(default)]
    pub parent_tool_use_id: Option<String>,
    // Derivation: CLAUDE-HEADLESS — "With --output-format json, the response
    // payload includes total_cost_usd ..." and stream-json's own terminal
    // line is "a result message with the final response text, cost, and
    // session metadata" — the same figure, on the same kind of message, over
    // the streaming transport.
    #[serde(default)]
    pub total_cost_usd: Option<f64>,
    // Derivation: CLAUDE-AGENT-SDK — `SDKResultMessage`'s TypeScript type
    // carries `duration_ms: number`, `duration_api_ms: number`, and
    // `num_turns: number` as siblings of `subtype` on both arms of its
    // union.
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub duration_api_ms: Option<u64>,
    #[serde(default)]
    pub num_turns: Option<u64>,
    // Derivation: CLAUDE-AGENT-SDK — `SDKResultMessage`'s union carries
    // `is_error: boolean` on both arms.
    #[serde(default)]
    pub is_error: Option<bool>,
    #[serde(flatten)]
    pub raw: Value,
}

impl Message {
    /// `type == "result"`: the terminal message of one invocation.
    // Derivation: CLAUDE-HEADLESS — "The last line of the stream is a result
    // message with the final response text, cost, and session metadata."
    pub fn is_result(&self) -> bool {
        self.kind == "result"
    }

    /// `type == "system", subtype == "init"`: the first message of a
    /// session (barring the plugin-install/hook-progress events the page
    /// names as preceding it, which this parser passes through as `Other`
    /// like any other `system` subtype).
    // Derivation: CLAUDE-HEADLESS — "The system/init event reports session
    // metadata ... It is the first event in the stream unless startup
    // events precede it."
    pub fn is_session_start(&self) -> bool {
        self.kind == "system" && self.subtype.as_deref() == Some("init")
    }

    /// A `result` message that reached its success arm — the run completed
    /// without an error. `false` for the error arm and for every non-`result`
    /// message alike.
    // Derivation: CLAUDE-AGENT-SDK — `SDKResultMessage`'s subtype union: the
    // success arm's `subtype: "success"` versus the error arm's `subtype:
    // "error_max_turns" | "error_during_execution" | "error_max_budget_usd"
    // | "error_max_structured_output_retries"`. `is_error` (also typed on
    // both arms) is the cheaper check when only success/failure matters and
    // the exact subtype string does not.
    pub fn is_successful_result(&self) -> bool {
        self.is_result() && self.subtype.as_deref() == Some("success")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MessageParseError {
    #[error("malformed stream-json line: {0}")]
    Json(String),
}

/// Parse one `stream-json` line. Never panics on malformed input — a bad
/// line is a typed error the caller decides what to do with (drop it, tear
/// down the session, …), not a crash.
pub fn parse_line(line: &[u8]) -> Result<Message, MessageParseError> {
    serde_json::from_slice(line).map_err(|e| MessageParseError::Json(e.to_string()))
}

/// The subagent fan-out tree, rebuilt by following `parent_tool_use_id`
/// chains. Keyed by branch id — a `tool_use` call's own id, which is the
/// value every message the spawned subagent produces carries back as
/// `parent_tool_use_id`.
///
/// Finding *which* branch issued a given `tool_use` id needs one more fact
/// than either permitted page states outright. `CLAUDE-HEADLESS` names
/// `tool_use` blocks as a thing the wire carries but never gives their JSON
/// shape, and `CLAUDE-AGENT-SDK`'s own message types stop at
/// `message: MessageParam; // From Anthropic SDK` — content blocks,
/// `tool_use` included, are typed by a third surface neither of this
/// crate's two rows names, so their shape stays out of citable reach.
/// `"type":"tool_use"` plus an `"id"` field is this crate's own inference —
/// a plausible tagged-block shape, not a claim either page makes about
/// field names — so it is an escalation on that specific point, not a
/// citation. A branch this walk cannot place is recorded as a root rather
/// than dropped or guessed at (see
/// `a_branch_with_no_recorded_issuing_tool_use_is_treated_as_a_root`).
// Derivation: CLAUDE-HEADLESS — "By default, Claude Code emits only
// subagent `tool_use` and `tool_result` blocks" (text/thinking blocks need
// `--forward-subagent-text`) establishes that a `tool_use` block is on the
// wire at all, and `parent_tool_use_id` is documented as "the ID of the
// tool call that spawned the subagent" — the same id space a `tool_use`
// call's own id lives in.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SubagentTree {
    /// branch id -> the branch that spawned it, `None` for a branch spawned
    /// directly off the main conversation.
    parent_branch: BTreeMap<String, Option<String>>,
}

impl SubagentTree {
    /// Rebuild the tree from one invocation's messages, in stream order.
    pub fn from_messages(messages: &[Message]) -> Self {
        // Which branch issued a given tool_use id — `None` if it was issued
        // from the main conversation.
        let mut issued_by: BTreeMap<String, Option<String>> = BTreeMap::new();
        for message in messages {
            for id in tool_use_ids(&message.raw) {
                issued_by
                    .entry(id)
                    .or_insert_with(|| message.parent_tool_use_id.clone());
            }
        }

        let mut parent_branch = BTreeMap::new();
        for message in messages {
            if let Some(branch) = &message.parent_tool_use_id {
                parent_branch
                    .entry(branch.clone())
                    .or_insert_with(|| issued_by.get(branch).cloned().flatten());
            }
        }
        Self { parent_branch }
    }

    /// The branch that spawned `branch`, or `None` if `branch` was spawned
    /// off the main conversation, or if `branch` was never observed at all
    /// — the two are indistinguishable from this tree alone; use
    /// [`Self::branches`] to tell them apart.
    pub fn parent_of(&self, branch: &str) -> Option<&str> {
        self.parent_branch.get(branch).and_then(Option::as_deref)
    }

    /// `branch` was observed and spawned directly off the main conversation.
    pub fn is_root(&self, branch: &str) -> bool {
        matches!(self.parent_branch.get(branch), Some(None))
    }

    /// Every branch this tree observed, in id order.
    pub fn branches(&self) -> impl Iterator<Item = &str> {
        self.parent_branch.keys().map(String::as_str)
    }
}

fn tool_use_ids(value: &Value) -> Vec<String> {
    let mut ids = Vec::new();
    collect_tool_use_ids(value, &mut ids);
    ids
}

fn collect_tool_use_ids(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            if map.get("type").and_then(Value::as_str) == Some("tool_use")
                && let Some(id) = map.get("id").and_then(Value::as_str)
            {
                out.push(id.to_owned());
            }
            for v in map.values() {
                collect_tool_use_ids(v, out);
            }
        }
        Value::Array(items) => {
            for v in items {
                collect_tool_use_ids(v, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_malformed_line_is_a_typed_error_not_a_panic() {
        let error = parse_line(b"not json").expect_err("malformed JSON must be refused");
        assert!(matches!(error, MessageParseError::Json(_)));
    }

    #[test]
    fn a_line_missing_type_is_a_typed_error() {
        let error =
            parse_line(br#"{"subtype":"init"}"#).expect_err("a missing `type` must be refused");
        assert!(matches!(error, MessageParseError::Json(_)));
    }

    #[test]
    fn system_init_is_session_start_and_result_is_terminal() {
        let init =
            parse_line(br#"{"type":"system","subtype":"init","session_id":"s1"}"#).expect("parse");
        assert!(init.is_session_start());
        assert!(!init.is_result());

        let result = parse_line(br#"{"type":"result","subtype":"success","total_cost_usd":0.42}"#)
            .expect("parse");
        assert!(result.is_result());
        assert!(!result.is_session_start());
        assert_eq!(result.total_cost_usd, Some(0.42));
    }

    #[test]
    fn a_success_result_carries_its_sdkresultmessage_fields_and_is_successful() {
        let result = parse_line(
            br#"{
                "type": "result",
                "subtype": "success",
                "duration_ms": 4200,
                "duration_api_ms": 3100,
                "num_turns": 3,
                "is_error": false,
                "total_cost_usd": 0.42
            }"#,
        )
        .expect("parse");
        assert!(result.is_successful_result());
        assert_eq!(result.duration_ms, Some(4200));
        assert_eq!(result.duration_api_ms, Some(3100));
        assert_eq!(result.num_turns, Some(3));
        assert_eq!(result.is_error, Some(false));
    }

    #[test]
    fn an_error_result_is_not_a_successful_result() {
        for subtype in [
            "error_max_turns",
            "error_during_execution",
            "error_max_budget_usd",
            "error_max_structured_output_retries",
        ] {
            let line = format!(r#"{{"type":"result","subtype":"{subtype}","is_error":true}}"#);
            let result = parse_line(line.as_bytes()).expect("parse");
            assert!(
                !result.is_successful_result(),
                "{subtype} must not be success"
            );
            assert_eq!(result.is_error, Some(true));
        }
    }

    #[test]
    fn unmodeled_fields_survive_in_raw() {
        // `stop_reason` and `permission_denials` are real SDKResultMessage
        // fields this crate does not (yet) type — proving the catch-all
        // still works for fields genuinely left unmodeled.
        let message = parse_line(br#"{"type":"result","stop_reason":"end_turn"}"#).expect("parse");
        assert_eq!(message.raw["stop_reason"], "end_turn");
    }

    #[test]
    fn main_conversation_messages_carry_no_parent() {
        let message =
            parse_line(br#"{"type":"assistant","parent_tool_use_id":null}"#).expect("parse");
        assert_eq!(message.parent_tool_use_id, None);
    }

    #[test]
    fn a_two_level_subagent_tree_reconstructs_from_parent_tool_use_id_chaining() {
        // Main conversation issues tool_use "agent-a" (an Agent call spawning
        // a subagent). That subagent's own messages carry
        // parent_tool_use_id = "agent-a", and one of them issues a NESTED
        // tool_use "agent-b", spawning a second-level subagent whose
        // messages carry parent_tool_use_id = "agent-b".
        let messages = vec![
            parse_line(
                br#"{"type":"assistant","parent_tool_use_id":null,"content":[{"type":"tool_use","id":"agent-a","name":"Agent"}]}"#,
            )
            .expect("parse"),
            parse_line(
                br#"{"type":"assistant","parent_tool_use_id":"agent-a","content":[{"type":"tool_use","id":"agent-b","name":"Agent"}]}"#,
            )
            .expect("parse"),
            parse_line(br#"{"type":"user","parent_tool_use_id":"agent-b"}"#).expect("parse"),
        ];

        let tree = SubagentTree::from_messages(&messages);
        assert!(tree.is_root("agent-a"));
        assert_eq!(tree.parent_of("agent-b"), Some("agent-a"));
        assert!(!tree.is_root("agent-b"));
        let mut branches: Vec<&str> = tree.branches().collect();
        branches.sort_unstable();
        assert_eq!(branches, ["agent-a", "agent-b"]);
    }

    #[test]
    fn a_branch_with_no_recorded_issuing_tool_use_is_treated_as_a_root() {
        // The issuing tool_use call was never observed (e.g. --forward-
        // subagent-text was off and only tool_result surfaced) — the branch
        // is still recorded, just as a root, rather than panicking or being
        // dropped.
        let messages =
            vec![parse_line(br#"{"type":"user","parent_tool_use_id":"orphan"}"#).expect("parse")];
        let tree = SubagentTree::from_messages(&messages);
        assert!(tree.is_root("orphan"));
    }
}
