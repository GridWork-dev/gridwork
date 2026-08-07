//! `gwk-pty-host`: the resident process. Installs the tracing sink, proves
//! kernel connectivity, tells a service manager it is ready, and waits to
//! be told to stop.
//!
//! The session runtime (spawn, pump loop, restart semantics, session
//! registry, detach/reattach routing) lives in this crate's library —
//! `gwk_pty_host::registry` down. The binary starts no session yet because
//! nothing can ask it to: the kernel-side hookup that routes attach and
//! spawn across the socket is the follow-up the crate root doc names, and a
//! session no consumer can reach would be a child process running for no
//! one. What the binary does today is exactly what its service unit needs
//! it to do; the registry joins this main loop the day the hookup lands.
//!
//! Derivation: none — process wiring and signal handling only; no terminal
//! byte is parsed and no engine process is supervised by this file.

use std::path::PathBuf;

/// Where the kernel's socket lives — the same lookup `gw`'s own CLI uses
/// (`crates/gridwork/src/main.rs`'s `socket_path`), so a deployment sets
/// one variable for every kernel client on the box rather than one per
/// binary.
fn socket_path() -> PathBuf {
    std::env::var_os(gwk_kernel::config::SOCKET_PATH_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(gwk_kernel::DEFAULT_SOCKET_PATH))
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let logger = gwk_pty_host::logging::TracingLogger::new(std::io::stdout(), tracing::Level::INFO);
    if let Err(e) = tracing::subscriber::set_global_default(logger) {
        // There is no logger yet to report THROUGH — the one place in this
        // binary a bare stderr line is the honest fallback rather than the
        // bug rule 4 of `identity/rust-discipline.md` exists to catch.
        eprintln!("gwk-pty-host: could not install the tracing sink: {e}");
    }

    let socket = socket_path();
    tracing::info!(socket = %socket.display(), "gwk-pty-host starting");

    match gwk_pty_host::kernel_client::KernelClient::connect(&socket).await {
        Ok(mut client) => match client.healthy().await {
            Ok(ready) => tracing::info!(ready, "kernel connectivity confirmed"),
            Err(error) => tracing::warn!(
                %error,
                "connected to the kernel but its health check failed"
            ),
        },
        Err(error) => tracing::warn!(%error, "could not reach the kernel at startup"),
    }

    // Reused rather than reimplemented: the exact `sd_notify` datagram this
    // process's own service unit expects, already implemented once for the
    // kernel daemon (`crates/gwk-kernel/src/wire/listen.rs`). A daemon
    // started by hand (no `NOTIFY_SOCKET`) sends nothing, which is not an
    // error.
    let notified = gwk_kernel::wire::listen::notify_ready();
    tracing::info!(notified, "readiness signaled");

    wait_for_stop().await;
    tracing::info!("gwk-pty-host stopping");
    std::process::ExitCode::SUCCESS
}

/// `SIGTERM` (a service manager's ask) or Ctrl-C, whichever arrives first —
/// the same two-signal posture `crates/gridwork/src/admin.rs`'s `daemon`
/// verb takes, and for the same reason: `ctrl_c()` alone never fires under
/// a systemd stop.
async fn wait_for_stop() {
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                term.recv().await;
            }
            // No handler could be installed: fall back to never resolving,
            // so Ctrl-C is still what stops this process rather than a
            // silent inability to.
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        () = terminate => {}
    }
}
