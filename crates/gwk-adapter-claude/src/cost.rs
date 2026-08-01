//! `result` → the kernel's spend ledger. Claude Code reports spend once per
//! invocation, never per subagent (`docs/PARITY.md` axis 3: "there is no
//! machine-readable per-child cost to demand"), so this module produces one
//! [`KernelCommand::RecordCostEntry`] per completed invocation — an
//! aggregate, never a running total and never a per-child breakdown this
//! engine cannot honestly report.

use gwk_domain::{
    AttemptId, CostEntryId, CostMicros, DispatchNodeId, EngineId, EngineSessionId, KernelCommand,
    TokenCount,
};

/// 1 USD in micro-USD — the ledger's unit ([`CostMicros`]).
const MICROS_PER_USD: f64 = 1_000_000.0;

#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
pub enum CostConversionError {
    #[error("total_cost_usd {0} is not a finite, non-negative dollar amount")]
    NotAFiniteNonNegativeAmount(f64),
}

/// Convert the engine's dollar figure to the ledger's integer micro-USD.
///
/// The conversion itself — dollars times a million, rounded — is this
/// crate's own arithmetic, not a vendor-documented formula; only the source
/// field is cited.
// Derivation: CLAUDE-HEADLESS — "With --output-format json, the response
// payload includes total_cost_usd and a per-model cost breakdown" and
// stream-json's terminal `result` line carries "the final response text,
// cost, and session metadata" — the same figure over the streaming
// transport.
pub fn usd_to_cost_micros(total_cost_usd: f64) -> Result<CostMicros, CostConversionError> {
    if !total_cost_usd.is_finite() || total_cost_usd < 0.0 {
        return Err(CostConversionError::NotAFiniteNonNegativeAmount(
            total_cost_usd,
        ));
    }
    let micros = (total_cost_usd * MICROS_PER_USD).round();
    // `u64::MAX` micro-USD is themselves ~18.4 trillion dollars — this can
    // only fire on already-nonsensical input, but the cast below is
    // infallible only once this is checked.
    if micros > u64::MAX as f64 {
        return Err(CostConversionError::NotAFiniteNonNegativeAmount(
            total_cost_usd,
        ));
    }
    Ok(CostMicros::new(micros as u64))
}

/// Token counts as the engine reported them on `result.usage`.
///
/// This module does not parse `usage` itself and asserts nothing about its
/// key names: none of the three permitted `CLAUDE-*` pages this adapter is
/// scoped to (`code.claude.com/docs/en/{cli-reference,headless,hooks}`)
/// states that object's shape — an escalation, not a guess (CLEANROOM.md
/// rule 3), recorded in the dispatch report rather than invented here. The
/// caller supplies whatever it has already parsed out of the raw message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResultUsage {
    pub input_tokens: Option<u64>,
    pub cached_input_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}

/// The identifying context a [`KernelCommand::RecordCostEntry`] needs beyond
/// the figures themselves. Every id here is caller-minted or caller-known —
/// this crate never generates one; id generation is the host's job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CostEntryContext {
    pub cost_entry_id: CostEntryId,
    pub attempt_id: Option<AttemptId>,
    pub engine_session_id: Option<EngineSessionId>,
    pub dispatch_node_id: Option<DispatchNodeId>,
    pub model: Option<String>,
}

/// Build the one `RecordCostEntry` a completed invocation's `result`
/// message yields.
///
/// `total_cost_usd` is always marked `cost_is_estimate: true` when present.
/// That characterization is the phase's own design contract
/// (`docs/PARITY.md`: "a client-side list-rate estimate with no stated
/// error bound") rather than a citable fact from the three permitted
/// pages — none of them uses the word "estimate" for this field — so it is
/// recorded here as a policy choice this constructor always applies, not as
/// a `Derivation:` claim.
pub fn record_cost_entry(
    context: CostEntryContext,
    total_cost_usd: Option<f64>,
    usage: ResultUsage,
) -> Result<KernelCommand, CostConversionError> {
    let cost_micros = total_cost_usd.map(usd_to_cost_micros).transpose()?;
    Ok(KernelCommand::RecordCostEntry {
        cost_entry_id: context.cost_entry_id,
        attempt_id: context.attempt_id,
        engine_session_id: context.engine_session_id,
        dispatch_node_id: context.dispatch_node_id,
        engine: EngineId::new(crate::ENGINE),
        model: context.model,
        input_tokens: usage.input_tokens.map(TokenCount::new),
        cached_input_tokens: usage.cached_input_tokens.map(TokenCount::new),
        cache_write_tokens: usage.cache_write_tokens.map(TokenCount::new),
        output_tokens: usage.output_tokens.map(TokenCount::new),
        reasoning_tokens: usage.reasoning_tokens.map(TokenCount::new),
        cost_micros,
        cost_is_estimate: cost_micros.is_some().then_some(true),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> CostEntryContext {
        CostEntryContext {
            cost_entry_id: CostEntryId::new("cost-1"),
            attempt_id: Some(AttemptId::new("att-1")),
            engine_session_id: Some(EngineSessionId::new("sess-1")),
            dispatch_node_id: None,
            model: Some("claude-opus".to_owned()),
        }
    }

    #[test]
    fn a_dollar_figure_converts_to_micro_usd_and_is_flagged_an_estimate() {
        let micros = usd_to_cost_micros(1.25).expect("valid amount");
        assert_eq!(micros, CostMicros::new(1_250_000));
    }

    #[test]
    fn rounding_is_to_the_nearest_micro_usd() {
        // 0.0000005 * 1_000_000 = 0.5, which rounds to 1, not 0 — round(),
        // not truncation.
        let micros = usd_to_cost_micros(0.000_000_5).expect("valid amount");
        assert_eq!(micros, CostMicros::new(1));
    }

    #[test]
    fn negative_nan_and_infinite_amounts_are_refused() {
        for bad in [-0.01, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let error = usd_to_cost_micros(bad).expect_err("invalid amount must be refused");
            assert!(matches!(
                error,
                CostConversionError::NotAFiniteNonNegativeAmount(_)
            ));
        }
    }

    #[test]
    fn a_result_becomes_a_cost_entry_with_the_estimate_flag_set() {
        let usage = ResultUsage {
            input_tokens: Some(1_200),
            cached_input_tokens: Some(800),
            cache_write_tokens: None,
            output_tokens: Some(300),
            reasoning_tokens: None,
        };
        let command = record_cost_entry(context(), Some(0.42), usage).expect("valid amount");
        let KernelCommand::RecordCostEntry {
            engine,
            model,
            input_tokens,
            cached_input_tokens,
            output_tokens,
            cost_micros,
            cost_is_estimate,
            ..
        } = command
        else {
            panic!("expected RecordCostEntry, got {command:?}");
        };
        assert_eq!(engine.as_str(), crate::ENGINE);
        assert_eq!(model, Some("claude-opus".to_owned()));
        assert_eq!(input_tokens, Some(TokenCount::new(1_200)));
        assert_eq!(cached_input_tokens, Some(TokenCount::new(800)));
        assert_eq!(output_tokens, Some(TokenCount::new(300)));
        assert_eq!(cost_micros, Some(CostMicros::new(420_000)));
        assert_eq!(cost_is_estimate, Some(true));
    }

    #[test]
    fn no_reported_cost_leaves_the_estimate_flag_absent_not_false() {
        let command = record_cost_entry(context(), None, ResultUsage::default())
            .expect("no amount to convert");
        let KernelCommand::RecordCostEntry {
            cost_micros,
            cost_is_estimate,
            ..
        } = command
        else {
            panic!("expected RecordCostEntry, got {command:?}");
        };
        assert_eq!(cost_micros, None);
        assert_eq!(cost_is_estimate, None);
    }

    #[test]
    fn a_malformed_amount_fails_the_whole_command_not_just_the_field() {
        let error = record_cost_entry(context(), Some(f64::NAN), ResultUsage::default())
            .expect_err("NaN must be refused");
        assert!(matches!(
            error,
            CostConversionError::NotAFiniteNonNegativeAmount(_)
        ));
    }
}
