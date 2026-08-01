//! Wire fact → typed [`KernelCommand`] value, and the reverse: a kernel-side
//! decision → the JSON-RPC response value that carries it back to the
//! engine.
//!
//! Everything here is a pure data transform. Nothing in this module decides
//! whether to grant an approval, what a cost entry's currency figure should
//! be, or which attempt a fact belongs to — those are kernel-side calls the
//! caller supplies as parameters. That split is the architecture boundary
//! this crate was scoped to: the adapter relays, the kernel decides.

use crate::schema::{
    CommandExecutionApprovalDecision, CommandExecutionRequestApprovalParams,
    CommandExecutionRequestApprovalResponse, FileChangeApprovalDecision,
    FileChangeRequestApprovalParams, FileChangeRequestApprovalResponse,
    ThreadTokenUsageUpdatedNotification,
};
use crate::wire::{Frame, JsonRpcId};
use gwk_domain::{
    AttemptId, CostEntryId, DispatchNodeId, EngineSessionId, GateId, KernelCommand, TokenCount,
};

/// `TokenCount` wraps a `u64`; the wire reports token counts as `i64` with
/// no documented lower bound (see `schema.rs`'s `TokenUsageBreakdown` note).
/// A negative count is nonsensical for something the ledger records as a
/// quantity, so it clamps to zero rather than wrapping — the ledger is
/// meant to record what the engine reported, not what a two's-complement
/// reinterpretation of a malformed report would say.
fn clamp_token_count(value: i64) -> TokenCount {
    TokenCount::new(u64::try_from(value).unwrap_or(0))
}

/// Build the `RecordCostEntry` command for one `thread/tokenUsage/updated`
/// notification.
///
/// Uses the notification's `last` breakdown, never `total`: `total` is the
/// thread's running cumulative figure (`schema.rs`'s `ThreadTokenUsage` doc),
/// and `RecordCostEntry` appends one row per call to this ledger's own
/// append-only table (`gwk_domain::command::KernelCommand::RecordCostEntry`'s
/// doc: "one engine cost report... at least one subject ref is required").
/// Recording `total` on every notification would re-append the thread's
/// entire cumulative spend on every turn, over-counting by every prior
/// turn's tokens each time.
pub fn record_cost_entry(
    cost_entry_id: CostEntryId,
    attempt_id: Option<AttemptId>,
    engine_session_id: Option<EngineSessionId>,
    notification: &ThreadTokenUsageUpdatedNotification,
) -> KernelCommand {
    let usage = &notification.token_usage.last;
    KernelCommand::RecordCostEntry {
        cost_entry_id,
        attempt_id,
        engine_session_id,
        dispatch_node_id: None::<DispatchNodeId>,
        engine: crate::engine_id(),
        // The wire carries no model identifier on this notification; a
        // caller that already knows which model the turn ran under
        // supplies it via a separate ingestion path, not invented here.
        model: None,
        input_tokens: Some(clamp_token_count(usage.input_tokens)),
        cached_input_tokens: Some(clamp_token_count(usage.cached_input_tokens)),
        cache_write_tokens: Some(clamp_token_count(usage.cache_write_input_tokens)),
        output_tokens: Some(clamp_token_count(usage.output_tokens)),
        reasoning_tokens: Some(clamp_token_count(usage.reasoning_output_tokens)),
        // Derivation: CODEX-APP-SERVER `schemas/v2/ThreadTokenUsageUpdatedNotification.json`
        // — `TokenUsageBreakdown` has no cost/currency field; `docs/PARITY.md`
        // axis 3 states this plainly ("no currency exists anywhere in the
        // protocol"). `cost_micros: None` records that absence rather than
        // inventing a conversion the ledger must never perform
        // (`gwk_domain::command::KernelCommand::RecordCostEntry`'s own doc,
        // echoed in `docs/PARITY.md`: "never invent a conversion").
        cost_micros: None,
        cost_is_estimate: None,
    }
}

/// The four plain decisions offered as a command-execution approval's
/// options, in a stable, sensible-to-a-human order. `availableDecisions`
/// wins when the engine names one; this is only the fallback when it does
/// not (the field is nullable — `schema.rs`'s
/// `CommandExecutionRequestApprovalParams` doc).
const DEFAULT_COMMAND_EXECUTION_OPTIONS: [CommandExecutionApprovalDecision; 4] =
    CommandExecutionApprovalDecision::PLAIN;

/// Build the `OpenGate` command for a `item/commandExecution/requestApproval`
/// request.
///
/// `options` only ever carries the four plain decision tags
/// (`accept`/`acceptForSession`/`decline`/`cancel`) — never
/// `acceptWithExecpolicyAmendment` or `applyNetworkPolicyAmendment`, even
/// when the engine lists one of those in `availableDecisions`. Both
/// structured variants require the amendment payload as part of the
/// decision (`execpolicy_amendment`/`network_policy_amendment`); a kernel
/// `OpenGate`'s `options` is `Vec<String>`, which has nowhere to carry that
/// payload, and inventing one here would mean deciding what amendment to
/// propose — a policy call, not a relay. A human presented with this gate
/// can still always fall back to a plain accept or decline.
pub fn open_gate_for_command_execution(
    gate_id: GateId,
    attempt_id: Option<AttemptId>,
    params: &CommandExecutionRequestApprovalParams,
) -> KernelCommand {
    let question = command_execution_question(params);
    let offered = params
        .available_decisions
        .as_deref()
        .unwrap_or(&DEFAULT_COMMAND_EXECUTION_OPTIONS);
    let options: Vec<String> = offered
        .iter()
        .filter_map(CommandExecutionApprovalDecision::wire_tag)
        .map(str::to_owned)
        .collect();
    // Every filtered-out list still resolves to something choosable: if the
    // engine's own `availableDecisions` list somehow named only structured
    // variants, fall back to the same default a `None` list would use.
    let options = if options.is_empty() {
        DEFAULT_COMMAND_EXECUTION_OPTIONS
            .iter()
            .filter_map(CommandExecutionApprovalDecision::wire_tag)
            .map(str::to_owned)
            .collect()
    } else {
        options
    };

    KernelCommand::OpenGate {
        gate_id,
        attempt_id,
        phase_ref: None,
        kind: Some("codex_command_execution_approval".to_owned()),
        question: Some(question),
        options: Some(options),
    }
}

fn command_execution_question(params: &CommandExecutionRequestApprovalParams) -> String {
    match (&params.command, &params.reason) {
        (Some(command), Some(reason)) => format!("Run `{command}`? ({reason})"),
        (Some(command), None) => format!("Run `{command}`?"),
        (None, Some(reason)) => format!("codex requests approval to run a command: {reason}"),
        (None, None) => "codex requests approval to run a command".to_owned(),
    }
}

/// Build the `OpenGate` command for a `item/fileChange/requestApproval`
/// request. Unlike command execution, the offered set is always the fixed
/// four-variant `FileChangeApprovalDecision` — the wire carries no
/// `availableDecisions` for this request kind (`schema.rs`'s
/// `FileChangeRequestApprovalParams` doc).
pub fn open_gate_for_file_change(
    gate_id: GateId,
    attempt_id: Option<AttemptId>,
    params: &FileChangeRequestApprovalParams,
) -> KernelCommand {
    let question = match &params.reason {
        Some(reason) => format!("codex requests approval to change files: {reason}"),
        None => "codex requests approval to change files".to_owned(),
    };
    let options = FileChangeApprovalDecision::ALL
        .iter()
        .map(|d| d.wire_tag().to_owned())
        .collect();
    KernelCommand::OpenGate {
        gate_id,
        attempt_id,
        phase_ref: None,
        kind: Some("codex_file_change_approval".to_owned()),
        question: Some(question),
        options: Some(options),
    }
}

/// Why a `chosen_option` could not become a JSON-RPC response.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DecisionMappingError {
    #[error("{chosen_option:?} is not one of the plain decision tags this relay offers")]
    UnknownOption { chosen_option: String },
}

/// The decision mapping back from a `chosen_option` to the JSON-RPC response
/// value for a resolved `item/commandExecution/requestApproval` request.
pub fn command_execution_response(
    request_id: JsonRpcId,
    chosen_option: &str,
) -> Result<Frame, DecisionMappingError> {
    let decision =
        CommandExecutionApprovalDecision::from_wire_tag(chosen_option).ok_or_else(|| {
            DecisionMappingError::UnknownOption {
                chosen_option: chosen_option.to_owned(),
            }
        })?;
    let response = CommandExecutionRequestApprovalResponse { decision };
    Ok(Frame::response(
        request_id,
        // `CommandExecutionRequestApprovalResponse` serializes with no
        // fallible field (no maps, no non-finite floats), so this cannot
        // fail in practice; `unwrap_or` over inventing a panic keeps the
        // signature honest without a reachable error arm to test.
        serde_json::to_value(response).unwrap_or(serde_json::Value::Null),
    ))
}

/// The decision mapping back from a `chosen_option` to the JSON-RPC response
/// value for a resolved `item/fileChange/requestApproval` request.
pub fn file_change_response(
    request_id: JsonRpcId,
    chosen_option: &str,
) -> Result<Frame, DecisionMappingError> {
    let decision = FileChangeApprovalDecision::from_wire_tag(chosen_option).ok_or_else(|| {
        DecisionMappingError::UnknownOption {
            chosen_option: chosen_option.to_owned(),
        }
    })?;
    let response = FileChangeRequestApprovalResponse { decision };
    Ok(Frame::response(
        request_id,
        serde_json::to_value(response).unwrap_or(serde_json::Value::Null),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::ThreadTokenUsage;
    use crate::wire::{JsonRpcId, ResponseFrame};

    fn token_usage_notification(last_output: i64) -> ThreadTokenUsageUpdatedNotification {
        ThreadTokenUsageUpdatedNotification {
            thread_id: "th-1".to_owned(),
            turn_id: "turn-1".to_owned(),
            token_usage: ThreadTokenUsage {
                last: crate::schema::TokenUsageBreakdown {
                    input_tokens: 100,
                    cached_input_tokens: 10,
                    cache_write_input_tokens: 0,
                    output_tokens: last_output,
                    reasoning_output_tokens: 5,
                    total_tokens: 100 + last_output + 5,
                },
                total: crate::schema::TokenUsageBreakdown {
                    input_tokens: 9_999,
                    cached_input_tokens: 10,
                    cache_write_input_tokens: 0,
                    output_tokens: 9_999,
                    reasoning_output_tokens: 5,
                    total_tokens: 30_000,
                },
                model_context_window: None,
            },
        }
    }

    #[test]
    fn record_cost_entry_uses_last_never_total() {
        let notification = token_usage_notification(20);
        let command = record_cost_entry(
            CostEntryId::new("cost-1"),
            Some(AttemptId::new("att-1")),
            None,
            &notification,
        );
        let KernelCommand::RecordCostEntry {
            input_tokens,
            output_tokens,
            cost_micros,
            cost_is_estimate,
            ..
        } = command
        else {
            panic!("expected RecordCostEntry");
        };
        assert_eq!(input_tokens.map(|t| t.value()), Some(100));
        assert_eq!(output_tokens.map(|t| t.value()), Some(20));
        assert_eq!(cost_micros, None, "no currency exists in this protocol");
        assert_eq!(cost_is_estimate, None);
    }

    #[test]
    fn record_cost_entry_clamps_a_negative_report_to_zero_rather_than_wrapping() {
        let mut notification = token_usage_notification(0);
        notification.token_usage.last.output_tokens = -5;
        let command = record_cost_entry(CostEntryId::new("cost-1"), None, None, &notification);
        let KernelCommand::RecordCostEntry { output_tokens, .. } = command else {
            panic!("expected RecordCostEntry");
        };
        assert_eq!(output_tokens.map(|t| t.value()), Some(0));
    }

    #[test]
    fn open_gate_for_command_execution_defaults_to_the_four_plain_options() {
        let params = CommandExecutionRequestApprovalParams {
            item_id: "item-1".to_owned(),
            thread_id: "th-1".to_owned(),
            turn_id: "turn-1".to_owned(),
            command: Some("rm -rf /tmp/x".to_owned()),
            reason: None,
            available_decisions: None,
        };
        let command = open_gate_for_command_execution(
            GateId::new("gate-1"),
            Some(AttemptId::new("att-1")),
            &params,
        );
        let KernelCommand::OpenGate {
            question, options, ..
        } = command
        else {
            panic!("expected OpenGate");
        };
        assert_eq!(question.as_deref(), Some("Run `rm -rf /tmp/x`?"));
        assert_eq!(
            options,
            Some(vec![
                "accept".to_owned(),
                "acceptForSession".to_owned(),
                "decline".to_owned(),
                "cancel".to_owned(),
            ])
        );
    }

    #[test]
    fn open_gate_for_command_execution_drops_amendment_variants_from_available_decisions() {
        let params = CommandExecutionRequestApprovalParams {
            item_id: "item-1".to_owned(),
            thread_id: "th-1".to_owned(),
            turn_id: "turn-1".to_owned(),
            command: Some("curl https://example.com".to_owned()),
            reason: None,
            available_decisions: Some(vec![
                CommandExecutionApprovalDecision::Accept,
                CommandExecutionApprovalDecision::ApplyNetworkPolicyAmendment {
                    network_policy_amendment: crate::schema::NetworkPolicyAmendment {
                        action: crate::schema::NetworkPolicyRuleAction::Allow,
                        host: "example.com".to_owned(),
                    },
                },
                CommandExecutionApprovalDecision::Decline,
            ]),
        };
        let command = open_gate_for_command_execution(GateId::new("gate-1"), None, &params);
        let KernelCommand::OpenGate { options, .. } = command else {
            panic!("expected OpenGate");
        };
        assert_eq!(
            options,
            Some(vec!["accept".to_owned(), "decline".to_owned()])
        );
    }

    #[test]
    fn open_gate_for_command_execution_falls_back_when_every_offered_decision_is_structured() {
        let params = CommandExecutionRequestApprovalParams {
            item_id: "item-1".to_owned(),
            thread_id: "th-1".to_owned(),
            turn_id: "turn-1".to_owned(),
            command: None,
            reason: None,
            available_decisions: Some(vec![
                CommandExecutionApprovalDecision::ApplyNetworkPolicyAmendment {
                    network_policy_amendment: crate::schema::NetworkPolicyAmendment {
                        action: crate::schema::NetworkPolicyRuleAction::Deny,
                        host: "example.com".to_owned(),
                    },
                },
            ]),
        };
        let command = open_gate_for_command_execution(GateId::new("gate-1"), None, &params);
        let KernelCommand::OpenGate { options, .. } = command else {
            panic!("expected OpenGate");
        };
        // A human is never left with nothing to click: the engine offered
        // only a structured decision this relay cannot represent as a bare
        // option, so the same four-way default a `None` list would use.
        assert_eq!(
            options,
            Some(vec![
                "accept".to_owned(),
                "acceptForSession".to_owned(),
                "decline".to_owned(),
                "cancel".to_owned(),
            ])
        );
    }

    #[test]
    fn open_gate_for_file_change_always_offers_the_fixed_four() {
        let params = FileChangeRequestApprovalParams {
            item_id: "item-2".to_owned(),
            thread_id: "th-1".to_owned(),
            turn_id: "turn-1".to_owned(),
            reason: Some("writes outside the workspace root".to_owned()),
            grant_root: None,
        };
        let command = open_gate_for_file_change(GateId::new("gate-2"), None, &params);
        let KernelCommand::OpenGate {
            question, options, ..
        } = command
        else {
            panic!("expected OpenGate");
        };
        assert_eq!(
            question.as_deref(),
            Some("codex requests approval to change files: writes outside the workspace root")
        );
        assert_eq!(
            options,
            Some(vec![
                "accept".to_owned(),
                "acceptForSession".to_owned(),
                "decline".to_owned(),
                "cancel".to_owned(),
            ])
        );
    }

    #[test]
    fn command_execution_response_maps_a_chosen_option_to_the_wire_decision() {
        let frame = command_execution_response(JsonRpcId::Str("req-1".to_owned()), "decline")
            .expect("known option");
        let Frame::Response(ResponseFrame { id, result }) = frame else {
            panic!("expected a Response frame");
        };
        assert_eq!(id, JsonRpcId::Str("req-1".to_owned()));
        assert_eq!(result, serde_json::json!({"decision": "decline"}));
    }

    #[test]
    fn file_change_response_maps_a_chosen_option_to_the_wire_decision() {
        let frame = file_change_response(JsonRpcId::Num(3), "accept").expect("known option");
        let Frame::Response(ResponseFrame { id, result }) = frame else {
            panic!("expected a Response frame");
        };
        assert_eq!(id, JsonRpcId::Num(3));
        assert_eq!(result, serde_json::json!({"decision": "accept"}));
    }

    #[test]
    fn an_unknown_chosen_option_is_a_typed_error() {
        let err = command_execution_response(JsonRpcId::Num(1), "acceptWithExecpolicyAmendment")
            .expect_err("not a plain-tag option");
        assert_eq!(
            err,
            DecisionMappingError::UnknownOption {
                chosen_option: "acceptWithExecpolicyAmendment".to_owned()
            }
        );

        let err = file_change_response(JsonRpcId::Num(1), "bogus").expect_err("not a known tag");
        assert_eq!(
            err,
            DecisionMappingError::UnknownOption {
                chosen_option: "bogus".to_owned()
            }
        );
    }
}
