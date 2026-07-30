//! The `gw` entry point. Everything it does is in the library beside it, so the
//! command line can be exercised without spawning a process.

use std::process::ExitCode;

/// A current-thread runtime: every verb here is one connection asking one
/// question, so a thread pool would be a pool of idle threads.
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    ExitCode::from(gridwork::run(&argv).await)
}
