//! Per-child cost and token extraction, and the spend-ledger command it
//! becomes.
//!
//! opencode's child sessions are first-class rows (`Session.parentID`,
//! `GET /session/{id}/children`), and each carries its own cost and token
//! usage on its own assistant messages (`AssistantMessage.cost`/`.tokens`).
//! This module turns that into [`gwk_domain::KernelCommand::RecordCostEntry`]
//! — never invents a conversion the engine did not report, per the ledger's
//! own contract (`docs/PARITY.md`, Axis 3).
//!
//! Every shape here — session listing, the message envelope, and the
//! `cost`/`tokens` fields inside it — comes from `OPENCODE-SERVER`
//! (opencode.ai/docs/server): the endpoint table for the two calls, and the
//! OpenAPI 3.1 schema that page's server publishes at `GET /doc` for the
//! field names inside `Message`. Residual honestly kept: this crate never
//! talks to a live engine, so the schema-derived field names are this
//! dispatch's best reading, not an independent re-derivation against a
//! running server — the parity harness settles that live, per
//! `docs/PARITY.md`.

use serde::Deserialize;

use gwk_domain::{
    AttemptId, CostEntryId, CostMicros, DispatchNodeId, EngineSessionId, KernelCommand, TokenCount,
};

/// One entry from `GET /session/:id/children`.
// Derivation: OPENCODE-SERVER §Sessions — `GET /session/:id/children`
// returns `Session[]`, and `POST /session` accepts a `parentID` on the
// session it creates — the two facts that make child sessions first-class
// rows this adapter can list and attribute.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ChildSession {
    pub id: String,
    #[serde(rename = "parentID", default)]
    pub parent_id: Option<String>,
}

/// Token counts as opencode breaks them down on one assistant message.
/// Absent fields are simply not reported — never coerced to zero, since
/// zero is a real count a caller must not confuse with "unknown."
// Derivation: OPENCODE-SERVER §Messages (OpenAPI schema at `GET /doc`) —
// `AssistantMessage.tokens`' breakdown: `input`, `output`, `reasoning`, and
// a `cache` pair (`cacheRead`, `cacheWrite` on the wire).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    #[serde(default)]
    pub input: Option<u64>,
    #[serde(default)]
    pub output: Option<u64>,
    #[serde(default)]
    pub reasoning: Option<u64>,
    #[serde(default)]
    pub cache_read: Option<u64>,
    #[serde(default)]
    pub cache_write: Option<u64>,
}

/// One assistant message's reported spend — the ledger's atom. `cost` is
/// dollars (opencode's own unit); [`dollars_to_micros`] is where it becomes
/// the ledger's micro-USD integer.
// Derivation: OPENCODE-SERVER §Messages (OpenAPI schema at `GET /doc`) —
// `AssistantMessage.cost` (dollars) and `.tokens` (the breakdown above).
#[derive(Debug, Clone, Copy, PartialEq, Default, Deserialize)]
pub struct AssistantMessageUsage {
    #[serde(default)]
    pub cost: Option<f64>,
    #[serde(default)]
    pub tokens: TokenUsage,
}

/// `GET /session/:id/message`'s per-message envelope. The response also
/// carries a sibling `parts` array this module has no use for — serde
/// ignores unknown fields by default, so it is simply not modeled here.
// Derivation: OPENCODE-SERVER §Messages — `GET /session/:id/message` returns
// `{ info: Message, parts: Part[] }[]`; this crate reads `info` for its cost
// and token facts (field names: see the `AssistantMessageUsage` citation).
#[derive(Debug, Clone, Deserialize)]
struct MessageEnvelope {
    info: AssistantMessageUsage,
}

/// Why session or message JSON did not parse.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[error("malformed opencode session/message JSON: {0}")]
pub struct CostParseError(String);

/// Parse a `GET /session/:id/children` response.
pub fn parse_children(json: &str) -> Result<Vec<ChildSession>, CostParseError> {
    serde_json::from_str(json).map_err(|e| CostParseError(e.to_string()))
}

/// Parse one `GET /session/:id/message` list entry.
pub fn parse_message_usage(json: &str) -> Result<AssistantMessageUsage, CostParseError> {
    let envelope: MessageEnvelope =
        serde_json::from_str(json).map_err(|e| CostParseError(e.to_string()))?;
    Ok(envelope.info)
}

/// Convert opencode's dollar-denominated cost into the ledger's micro-USD
/// integer (`1_000_000 = $1`), rounding to the nearest micro-cent. `None`
/// for a negative, non-finite, or unrepresentably large amount — the
/// ledger records what the engine reported, and a broken float is not a
/// report.
pub fn dollars_to_micros(dollars: f64) -> Option<CostMicros> {
    if !dollars.is_finite() || dollars < 0.0 {
        return None;
    }
    let micros = (dollars * 1_000_000.0).round();
    if micros > u64::MAX as f64 {
        return None;
    }
    // ponytail: `as` after both the finiteness and range checks above is the
    // whole conversion — a checked path through `u64::try_from` would reject
    // exactly the same values this already refused.
    Some(CostMicros::new(micros as u64))
}

fn add_opt(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))),
    }
}

/// One session's total spend and usage — the sum of every assistant message
/// reported inside it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SessionCost {
    pub cost_usd: Option<f64>,
    pub tokens: TokenUsage,
}

/// Sum a child session's own reported messages into one [`SessionCost`].
/// `GET /session/{parent}/children` names the sessions; each child's own
/// `GET /session/{child}/message` supplies the facts this sums.
pub fn sum_child_cost(messages: &[AssistantMessageUsage]) -> SessionCost {
    let mut total = SessionCost::default();
    for message in messages {
        if let Some(cost) = message.cost {
            total.cost_usd = Some(total.cost_usd.unwrap_or(0.0) + cost);
        }
        total.tokens.input = add_opt(total.tokens.input, message.tokens.input);
        total.tokens.output = add_opt(total.tokens.output, message.tokens.output);
        total.tokens.reasoning = add_opt(total.tokens.reasoning, message.tokens.reasoning);
        total.tokens.cache_read = add_opt(total.tokens.cache_read, message.tokens.cache_read);
        total.tokens.cache_write = add_opt(total.tokens.cache_write, message.tokens.cache_write);
    }
    total
}

/// Build the `RecordCostEntry` command one session's summed usage warrants.
/// The caller supplies the kernel-scoped identifiers this adapter cannot
/// know on its own.
pub fn record_cost_entry_command(
    cost_entry_id: CostEntryId,
    attempt_id: Option<AttemptId>,
    engine_session_id: Option<EngineSessionId>,
    dispatch_node_id: Option<DispatchNodeId>,
    model: Option<String>,
    cost: &SessionCost,
) -> KernelCommand {
    KernelCommand::RecordCostEntry {
        cost_entry_id,
        attempt_id,
        engine_session_id,
        dispatch_node_id,
        engine: crate::engine_id(),
        model,
        input_tokens: cost.tokens.input.map(TokenCount::new),
        cached_input_tokens: cost.tokens.cache_read.map(TokenCount::new),
        cache_write_tokens: cost.tokens.cache_write.map(TokenCount::new),
        output_tokens: cost.tokens.output.map(TokenCount::new),
        reasoning_tokens: cost.tokens.reasoning.map(TokenCount::new),
        cost_micros: cost.cost_usd.and_then(dollars_to_micros),
        // Never asserted: opencode's docs, unlike Claude Code's, do not say
        // whether this figure is an estimate or a ledger fact, and the
        // ledger records what the engine reported rather than a guess about
        // it.
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
                },
                ChildSession {
                    id: "ses_child_2".to_owned(),
                    parent_id: Some("ses_parent".to_owned()),
                },
            ]
        );
    }

    #[test]
    fn a_message_reports_its_own_cost_and_token_breakdown() {
        let usage = parse_message_usage(
            r#"{"info":{"cost":0.0123,"tokens":{"input":100,"output":40,"reasoning":5,"cacheRead":10,"cacheWrite":2}},"parts":[]}"#,
        )
        .expect("parses");
        assert_eq!(usage.cost, Some(0.0123));
        assert_eq!(usage.tokens.input, Some(100));
        assert_eq!(usage.tokens.cache_read, Some(10));
        assert_eq!(usage.tokens.cache_write, Some(2));
    }

    #[test]
    fn dollar_cost_rounds_to_the_nearest_micro_cent() {
        assert_eq!(dollars_to_micros(1.0), Some(CostMicros::new(1_000_000)));
        assert_eq!(dollars_to_micros(0.000_001_4), Some(CostMicros::new(1)));
        assert_eq!(dollars_to_micros(0.0), Some(CostMicros::new(0)));
    }

    #[test]
    fn a_broken_dollar_amount_is_refused_not_coerced() {
        assert_eq!(dollars_to_micros(-0.01), None);
        assert_eq!(dollars_to_micros(f64::NAN), None);
        assert_eq!(dollars_to_micros(f64::INFINITY), None);
    }

    #[test]
    fn per_child_cost_extraction_sums_every_message_in_the_session() {
        let messages = [
            AssistantMessageUsage {
                cost: Some(0.01),
                tokens: TokenUsage {
                    input: Some(100),
                    output: Some(50),
                    reasoning: None,
                    cache_read: Some(10),
                    cache_write: None,
                },
            },
            AssistantMessageUsage {
                cost: Some(0.02),
                tokens: TokenUsage {
                    input: Some(200),
                    output: Some(75),
                    reasoning: Some(5),
                    cache_read: None,
                    cache_write: Some(3),
                },
            },
        ];
        let total = sum_child_cost(&messages);
        assert!((total.cost_usd.expect("some cost") - 0.03).abs() < f64::EPSILON);
        assert_eq!(total.tokens.input, Some(300));
        assert_eq!(total.tokens.output, Some(125));
        assert_eq!(total.tokens.reasoning, Some(5));
        assert_eq!(total.tokens.cache_read, Some(10));
        assert_eq!(total.tokens.cache_write, Some(3));
    }

    #[test]
    fn a_session_with_no_reported_cost_stays_absent_not_zero() {
        let messages = [AssistantMessageUsage {
            cost: None,
            tokens: TokenUsage::default(),
        }];
        let total = sum_child_cost(&messages);
        assert_eq!(total.cost_usd, None);
        assert_eq!(total.tokens.input, None);
    }

    #[test]
    fn record_cost_entry_command_carries_the_engine_and_the_converted_spend() {
        let cost = SessionCost {
            cost_usd: Some(1.5),
            tokens: TokenUsage {
                input: Some(1000),
                output: Some(200),
                reasoning: None,
                cache_read: None,
                cache_write: None,
            },
        };
        let command = record_cost_entry_command(
            CostEntryId::new("cost-1"),
            Some(AttemptId::new("att-1")),
            None,
            None,
            Some("claude-sonnet".to_owned()),
            &cost,
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
        assert_eq!(cost_micros, Some(CostMicros::new(1_500_000)));
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
        assert!(matches!(
            parse_message_usage(r#"{"parts":[]}"#),
            Err(CostParseError(_))
        ));
    }
}
