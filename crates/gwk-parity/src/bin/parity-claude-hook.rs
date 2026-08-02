//! The `PreToolUse` hook binary `gwk-parity`'s Claude approval-relay runner
//! spawns as a genuinely separate process (`gwk_adapter_claude::relay`'s own
//! doc: "a `PreToolUse` invocation is a fresh, short-lived process whose
//! only decision-returning path is its OWN stdout").
//!
//! It does exactly what a real Claude Code `PreToolUse` hook subprocess
//! would: read one JSON line on stdin, decode it as a
//! [`gwk_adapter_claude::hook::PreToolUseAsk`], forward it to the relay
//! socket named by `GWK_PARITY_RELAY_SOCKET`, and write the relay's decision
//! to stdout as the hook's own documented stdout contract
//! (`HookDecision::to_stdout_json`). Every step goes through the adapter's
//! public types; this binary invents nothing about the wire.
//!
//! `main` is the one place in this crate allowed to be "the end of the
//! line" per `identity/rust-discipline.md` — a hook subprocess that cannot
//! read its own stdin or find its socket env var has nothing left to do but
//! report the failure and exit non-zero.

use std::io::Read;
use std::time::Duration;

use gwk_adapter_claude::hook::PreToolUseAsk;
use gwk_adapter_claude::relay::relay_ask;

const RELAY_DEADLINE: Duration = Duration::from_secs(30);

fn main() {
    if let Err(message) = run() {
        eprintln!("parity-claude-hook: {message}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let socket_path = std::env::var("GWK_PARITY_RELAY_SOCKET")
        .map_err(|_| "GWK_PARITY_RELAY_SOCKET is not set".to_owned())?;

    let mut stdin_bytes = Vec::new();
    std::io::stdin()
        .read_to_end(&mut stdin_bytes)
        .map_err(|e| format!("reading stdin: {e}"))?;
    let ask =
        PreToolUseAsk::from_stdin(&stdin_bytes).map_err(|e| format!("decoding the ask: {e}"))?;

    let decision = relay_ask(std::path::Path::new(&socket_path), &ask, RELAY_DEADLINE);
    let json = decision.to_stdout_json();
    println!(
        "{}",
        serde_json::to_string(&json).map_err(|e| format!("encoding the decision: {e}"))?
    );
    Ok(())
}
