//! Workspace task runner. Codegen (Rust -> TypeScript contract export) and the
//! schema `--check` gate land here as the contract work begins.

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some(task) => {
            eprintln!("unknown task: {task}");
            std::process::exit(2);
        }
        None => eprintln!("usage: cargo run -p xtask -- <task>   (no tasks defined yet)"),
    }
}
