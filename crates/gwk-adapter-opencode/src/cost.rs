//! Per-child cost and token extraction, and the spend-ledger command it
//! becomes.
//!
//! opencode's child sessions are first-class rows (`Session.parentID`,
//! `GET /session/{id}/children`), and each carries its own rolled-up cost
//! and token usage directly (`Session.cost`/`.tokens` — no separate
//! per-message summing needed). This module turns that into
//! [`gwk_domain::KernelCommand::RecordCostEntry`] — never inventing a
//! conversion the engine did not report, per the ledger's own contract
//! (`docs/PARITY.md`, Axis 3).
//!
//! Every shape here comes from `OPENCODE-SERVER` (opencode.ai/docs/server):
//! the endpoint table for `GET /session/{sessionID}/children`, and the
//! OpenAPI 3.1 schema that page's server publishes at `GET /doc` for
//! `Session`'s field names — read directly out of that schema (fetched from
//! a local `opencode 1.18.3` instance, `opencode serve` on a loopback port,
//! `curl .../doc`, then killed; no session was ever created).
//!
//! `Session.cost` is typed `number` with no stated unit anywhere this crate
//! can cite — not "dollars," not anything else. This module therefore never
//! converts it: the raw figure travels on [`ChildSession::cost`] for a
//! caller to interpret once the live harness settles what it actually
//! denominates, and [`record_cost_entry_command`] always leaves
//! `cost_micros` absent. Token counts carry no such ambiguity — they are
//! typed as plain counts and are the accounting fact regardless.

use serde::Deserialize;

use gwk_domain::{
    AttemptId, CostEntryId, DispatchNodeId, EngineSessionId, KernelCommand, TokenCount,
};

/// One cache token count.
// Derivation: OPENCODE-SERVER §Sessions (OpenAPI schema at `GET /doc`) —
// `Session.tokens.cache`: `{ read, write }`, both required whenever `cache`
// itself is present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct TokenCacheUsage {
    pub read: u64,
    pub write: u64,
}

/// Token counts as opencode rolls them up on a session.
// Derivation: OPENCODE-SERVER §Sessions (OpenAPI schema at `GET /doc`) —
// `Session.tokens`: `{ input, output, reasoning, cache }`, all four required
// whenever `tokens` itself is present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub reasoning: u64,
    pub cache: TokenCacheUsage,
}

/// One entry from `GET /session/:id/children` — a full `Session` row,
/// reduced to what this crate reads off it.
// Derivation: OPENCODE-SERVER §Sessions (OpenAPI schema at `GET /doc`) —
// `GET /session/{sessionID}/children` returns `Session[]`; `Session.id` and
// `.parentID` are how a child names itself and its parent, `.cost`/`.tokens`
// are its own rolled-up usage. All three of `parentID`/`cost`/`tokens` are
// optional on the schema.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ChildSession {
    pub id: String,
    #[serde(rename = "parentID", default)]
    pub parent_id: Option<String>,
    #[serde(default)]
    pub cost: Option<f64>,
    #[serde(default)]
    pub tokens: Option<TokenUsage>,
}

/// Why session JSON did not parse.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("malformed opencode session JSON: {0}")]
pub struct CostParseError(String);

/// Parse a `GET /session/:id/children` response.
pub fn parse_children(json: &str) -> Result<Vec<ChildSession>, CostParseError> {
    serde_json::from_str(json).map_err(|e| CostParseError(e.to_string()))
}

/// Build the `RecordCostEntry` command one child session's own rolled-up
/// usage warrants. The caller supplies the kernel-scoped identifiers this
/// adapter cannot know on its own.
pub fn record_cost_entry_command(
    cost_entry_id: CostEntryId,
    attempt_id: Option<AttemptId>,
    engine_session_id: Option<EngineSessionId>,
    dispatch_node_id: Option<DispatchNodeId>,
    model: Option<String>,
    child: &ChildSession,
) -> KernelCommand {
    KernelCommand::RecordCostEntry {
        cost_entry_id,
        attempt_id,
        engine_session_id,
        dispatch_node_id,
        engine: crate::engine_id(),
        model,
        input_tokens: child.tokens.map(|t| TokenCount::new(t.input)),
        cached_input_tokens: child.tokens.map(|t| TokenCount::new(t.cache.read)),
        cache_write_tokens: child.tokens.map(|t| TokenCount::new(t.cache.write)),
        output_tokens: child.tokens.map(|t| TokenCount::new(t.output)),
        reasoning_tokens: child.tokens.map(|t| TokenCount::new(t.reasoning)),
        // Never converted: see the module note on `Session.cost`'s
        // unverified unit. The ledger records what the engine reported, and
        // an uncited unit is not a report.
        cost_micros: None,
        cost_is_estimate: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_sessions_carry_their_parent() {
        let children = parse_children(
            r#"[{"id":"ses_child_1","parentID":"ses_parent"},{"id":"ses_child_2","parentID":"ses_parent"}]"#,
        )
        .expect("parses");
        assert_eq!(
            children,
            vec![
                ChildSession {
                    id: "ses_child_1".to_owned(),
                    parent_id: Some("ses_parent".to_owned()),
                    cost: None,
                    tokens: None,
                },
                ChildSession {
                    id: "ses_child_2".to_owned(),
                    parent_id: Some("ses_parent".to_owned()),
                    cost: None,
                    tokens: None,
                },
            ]
        );
    }

    #[test]
    fn per_child_cost_extraction_reads_the_rolled_up_figure_and_nested_cache_tokens() {
        let children = parse_children(
            r#"[{"id":"ses_child_1","parentID":"ses_parent","cost":0.0123,"tokens":{"input":100,"output":40,"reasoning":5,"cache":{"read":10,"write":2}}}]"#,
        )
        .expect("parses");
        let child = &children[0];
        assert_eq!(child.cost, Some(0.0123));
        let tokens = child.tokens.expect("tokens present");
        assert_eq!(tokens.input, 100);
        assert_eq!(tokens.output, 40);
        assert_eq!(tokens.reasoning, 5);
        assert_eq!(tokens.cache.read, 10);
        assert_eq!(tokens.cache.write, 2);
    }

    #[test]
    fn a_child_session_with_no_reported_usage_stays_absent_not_zero() {
        let children = parse_children(r#"[{"id":"ses_child_1"}]"#).expect("parses");
        assert_eq!(children[0].cost, None);
        assert_eq!(children[0].tokens, None);
    }

    #[test]
    fn record_cost_entry_command_carries_tokens_and_never_invents_a_cost_conversion() {
        let child = ChildSession {
            id: "ses_child_1".to_owned(),
            parent_id: Some("ses_parent".to_owned()),
            cost: Some(1.5),
            tokens: Some(TokenUsage {
                input: 1000,
                output: 200,
                reasoning: 0,
                cache: TokenCacheUsage { read: 0, write: 0 },
            }),
        };
        let command = record_cost_entry_command(
            CostEntryId::new("cost-1"),
            Some(AttemptId::new("att-1")),
            None,
            None,
            Some("claude-sonnet".to_owned()),
            &child,
        );
        let KernelCommand::RecordCostEntry {
            engine,
            cost_micros,
            input_tokens,
            output_tokens,
            cost_is_estimate,
            ..
        } = command
        else {
            panic!("expected RecordCostEntry");
        };
        assert_eq!(engine, crate::engine_id());
        // `child.cost` was `Some(1.5)` — still never converted, per the
        // module's disclosed unit-unverified residual.
        assert_eq!(cost_micros, None);
        assert_eq!(input_tokens, Some(TokenCount::new(1000)));
        assert_eq!(output_tokens, Some(TokenCount::new(200)));
        assert_eq!(cost_is_estimate, None);
    }

    #[test]
    fn malformed_session_json_is_a_typed_error_not_a_panic() {
        assert!(matches!(
            parse_children("{not json}"),
            Err(CostParseError(_))
        ));
        // The old flat `cacheRead`/`cacheWrite` shape no longer matches the
        // real nested `cache: { read, write }` schema — this is exactly the
        // bug the second reader caught, kept here as a regression case.
        assert!(matches!(
            parse_children(
                r#"[{"id":"ses_1","tokens":{"input":1,"output":1,"reasoning":0,"cacheRead":1,"cacheWrite":1}}]"#
            ),
            Err(CostParseError(_))
        ));
    }
}
