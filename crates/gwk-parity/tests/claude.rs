//! `#[ignore]`d local runners for Claude Code — compiled by public CI
//! (`docs/PARITY.md`: "public CI proves the adapter crates build"), never
//! run there. Run locally with:
//!
//! ```text
//! cargo test -p gwk-parity --test claude -- --ignored --test-threads=1
//! ```
//!
//! `--test-threads=1` matters here specifically: the approval-relay test
//! binds a Unix socket and spawns the crate's own hook binary, and running
//! it alongside another test in the same process is needless contention for
//! no speed win on a four-test file.
//!
//! Each test panics on [`gwk_parity::matrix::Verdict::Fail`] (a real
//! adapter regression) but accepts [`gwk_parity::matrix::Verdict::Pass`] or
//! [`gwk_parity::matrix::Verdict::Skipped`] (the engine legitimately was not
//! available, or — for the two documented partial runners — a live
//! session-driven trigger was out of the adapter's current public surface).

use gwk_parity::matrix::Verdict;
use gwk_parity::runners::claude;

fn assert_not_failed(cell: gwk_parity::matrix::Cell) {
    assert_ne!(
        cell.verdict,
        Verdict::Fail,
        "{} / {}: {}",
        cell.engine,
        cell.axis,
        cell.detail
    );
    eprintln!(
        "{} / {}: {} — {}",
        cell.engine, cell.axis, cell.verdict, cell.detail
    );
}

#[tokio::test]
#[ignore]
async fn lifecycle() {
    assert_not_failed(claude::lifecycle().await);
}

#[tokio::test]
#[ignore]
async fn status_truth() {
    assert_not_failed(claude::status_truth().await);
}

#[tokio::test]
#[ignore]
async fn transcript_ingestion() {
    assert_not_failed(claude::transcript_ingestion().await);
}

#[tokio::test]
#[ignore]
async fn approval_relay() {
    assert_not_failed(claude::approval_relay().await);
}
