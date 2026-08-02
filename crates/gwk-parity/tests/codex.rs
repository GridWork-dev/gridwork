//! `#[ignore]`d local runners for Codex — compiled by public CI, never run
//! there. Run locally with:
//!
//! ```text
//! cargo test -p gwk-parity --test codex -- --ignored
//! ```

use gwk_parity::matrix::Verdict;
use gwk_parity::runners::codex;

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
    assert_not_failed(codex::lifecycle().await);
}

#[tokio::test]
#[ignore]
async fn status_truth() {
    assert_not_failed(codex::status_truth().await);
}

#[tokio::test]
#[ignore]
async fn transcript_ingestion() {
    assert_not_failed(codex::transcript_ingestion().await);
}

#[tokio::test]
#[ignore]
async fn approval_relay() {
    assert_not_failed(codex::approval_relay().await);
}
