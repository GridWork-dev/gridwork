//! Workspace task runner. `contract` generates the TypeScript contract, theme
//! data, golden fixtures, and the kernel's embedded copy of the contract DDL;
//! `contract --check` is the CI drift gate.

mod contract;
mod schema;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("contract") => {
            let check = args.iter().any(|a| a == "--check");
            contract::run(check);
        }
        Some(task) => {
            eprintln!("unknown task: {task}");
            eprintln!("usage: cargo run -p xtask -- contract [--check]");
            std::process::exit(2);
        }
        None => {
            eprintln!("usage: cargo run -p xtask -- contract [--check]");
            std::process::exit(2);
        }
    }
}
