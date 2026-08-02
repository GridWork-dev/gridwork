//! The local driver: run every axis for every engine and print the filled
//! matrix. `docs/PARITY.md`: "The matrix is filled by a local runner that
//! drives all four axes per engine and emits the filled table."
//!
//! This never runs in CI (`docs/PARITY.md`'s own rule: "the matrix runs
//! locally, never in public CI"); it is the operator's own
//! `cargo run -p gwk-parity --bin parity` on a box with the three engine
//! CLIs logged in.

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let matrix = gwk_parity::runners::run_all().await;
    println!("{}", matrix.render_table());
    println!(
        "{}",
        serde_json::to_string_pretty(&matrix.to_json())
            .unwrap_or_else(|e| format!("{{\"error\":{e:?}}}"))
    );
    if !matrix.all_green() {
        std::process::exit(1);
    }
}
