//! `gwk-cert <stream.json>` — certify an exported event stream against the
//! contract. Findings print to stdout as a JSON array (stable, typed — CI
//! parses them); the human summary goes to stderr. Exit 0 = certified,
//! 1 = findings, 2 = usage.
//!
//! Non-claim: a coherent FORGED stream passes — tamper evidence lives in the
//! storage layer, not in stream inspection.

use gwk_cert::check::{check_stream, parse_stream};

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: gwk-cert <stream.json>");
        std::process::exit(2);
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(err) => {
            eprintln!("gwk-cert: cannot read {path}: {err}");
            std::process::exit(2);
        }
    };
    let (events, mut findings) = parse_stream(&raw);
    findings.extend(check_stream(&events));

    println!(
        "{}",
        serde_json::to_string_pretty(&findings).expect("findings serialize")
    );
    if findings.is_empty() {
        eprintln!("gwk-cert: certified — {} events, 0 findings", events.len());
    } else {
        eprintln!(
            "gwk-cert: NOT certified — {} events, {} finding(s)",
            events.len(),
            findings.len()
        );
        std::process::exit(1);
    }
}
