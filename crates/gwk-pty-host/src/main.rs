//! `gwk-pty-host`: the resident process. Installs the tracing sink, proves
//! kernel connectivity, starts every operator-declared session, publishes
//! each across the kernel socket, tells a service manager it is ready, and
//! waits to be told to stop.
//!
//! The registry joined this main loop when the attach hookup landed: each
//! declared session runs under [`gwk_pty_host::registry`], and a
//! [`gwk_pty_host::publish`] task per session carries its snapshot and
//! delta batches plus the raw fallback to the kernel, where render and raw
//! attaches serve them to any consumer on the socket. What no one declares, this binary still
//! does not run — an empty declaration is a legal resident state, and was
//! the only state before the hookup existed.
//!
//! Derivation: none — process wiring and signal handling only; no terminal
//! byte is parsed and no engine process is supervised by this file (the
//! registry and session modules own that).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gwk_domain::ids::PtySessionId;
use gwk_pty_host::publish::{self, SessionDecl};
use gwk_pty_host::registry::SessionRegistry;
use tokio::sync::{Mutex, watch};

/// Where the kernel's socket lives — the same lookup `gw`'s own CLI uses
/// (`crates/gridwork/src/lib.rs`'s `socket_path`), so a deployment sets
/// one variable for every kernel client on the box rather than one per
/// binary.
fn socket_path() -> PathBuf {
    std::env::var_os(gwk_kernel::config::SOCKET_PATH_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(gwk_kernel::DEFAULT_SOCKET_PATH))
}

/// How long shutdown waits for the publishers' parting retires before the
/// process exits anyway — the kernel retires on hangup regardless, so this
/// buys a typed close for consumers, not correctness.
const PUBLISHER_DRAIN: Duration = Duration::from_secs(5);

/// How long shutdown waits for the sessions themselves — child kills and
/// thread joins — before exiting anyway.
const SESSION_STOP: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let logger = gwk_pty_host::logging::TracingLogger::new(std::io::stdout(), tracing::Level::INFO);
    if let Err(e) = tracing::subscriber::set_global_default(logger) {
        // There is no logger yet to report THROUGH — the one place in this
        // binary a bare stderr line is the honest fallback rather than the
        // bug rule 4 of `identity/rust-discipline.md` exists to catch.
        eprintln!("gwk-pty-host: could not install the tracing sink: {e}");
    }

    // The operator's declarations, refused loudly and whole: a service that
    // came up serving half of what its unit says would fail quieter than one
    // that did not come up. `var_os` so a non-UTF8 value is ALSO loud —
    // `var()`'s error would fold it into "unset" and silently run nothing.
    let declared = match std::env::var_os(publish::SESSIONS_ENV) {
        None => String::new(),
        Some(value) => match value.into_string() {
            Ok(value) => value,
            Err(_) => {
                tracing::error!(
                    env = publish::SESSIONS_ENV,
                    "session declaration refused: the value is not UTF-8"
                );
                return std::process::ExitCode::FAILURE;
            }
        },
    };
    let declared = match publish::parse_sessions(&declared) {
        Ok(declared) => declared,
        Err(why) => {
            tracing::error!(%why, env = publish::SESSIONS_ENV, "session declaration refused");
            return std::process::ExitCode::FAILURE;
        }
    };

    let socket = socket_path();
    tracing::info!(
        socket = %socket.display(),
        sessions = declared.len(),
        "gwk-pty-host starting"
    );

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

    let registry = Arc::new(Mutex::new(SessionRegistry::new()));
    for decl in &declared {
        if let Err(why) = start_session(&registry, decl).await {
            tracing::error!(session = %decl.id, %why, "a declared session could not start");
            stop_sessions(&registry).await;
            return std::process::ExitCode::FAILURE;
        }
        tracing::info!(session = %decl.id, engine = %decl.engine, "session started");
    }

    let (stop_tx, stop_rx) = watch::channel(false);
    let publishers = Arc::new(Mutex::new(tokio::task::JoinSet::new()));
    let start_manager = tokio::spawn(gwk_pty_host::control::serve_starts(
        socket.clone(),
        Arc::clone(&registry),
        Arc::clone(&publishers),
        stop_rx.clone(),
    ));
    let reaper = tokio::spawn(gwk_pty_host::control::serve_reaper(
        Arc::clone(&registry),
        Arc::clone(&publishers),
        stop_rx.clone(),
    ));
    for decl in &declared {
        // Each publisher gets its own attach handle instead of the shared
        // registry: its attach round-trips through the session's thread, and
        // a shared lock held across that would let one stuck session starve
        // every other publisher's reconnect.
        let attacher = match registry.lock().await.attacher(&decl.id) {
            Ok(attacher) => attacher,
            Err(why) => {
                tracing::error!(session = %decl.id, %why, "a started session vanished");
                stop_sessions(&registry).await;
                return std::process::ExitCode::FAILURE;
            }
        };
        publishers.lock().await.spawn(publish::publish_session(
            attacher,
            socket.clone(),
            decl.id.clone(),
            stop_rx.clone(),
        ));
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

    // Order matters: sessions stop FIRST, so their broadcasts close and each
    // publisher's last act is a typed retire rather than a mid-stream hangup;
    // the drain is bounded because the hangup retires just as surely. The
    // stop itself is bounded too — it joins each session's thread, and a
    // child that will not reap must not hold shutdown hostage (the service
    // manager's stop timeout would eventually win, but by SIGKILL).
    let _ = stop_tx.send(true);
    if tokio::time::timeout(SESSION_STOP, stop_sessions(&registry))
        .await
        .is_err()
    {
        tracing::warn!("session shutdown exceeded its bound; exiting anyway");
    }
    let _ = tokio::time::timeout(PUBLISHER_DRAIN, start_manager).await;
    let _ = tokio::time::timeout(PUBLISHER_DRAIN, reaper).await;
    let _ = tokio::time::timeout(PUBLISHER_DRAIN, async {
        let mut publishers = publishers.lock().await;
        while publishers.join_next().await.is_some() {}
    })
    .await;
    std::process::ExitCode::SUCCESS
}

/// Start one declared session, resolving its engine to the adapter's own
/// spawn function.
async fn start_session(
    registry: &Arc<Mutex<SessionRegistry>>,
    decl: &SessionDecl,
) -> Result<(), String> {
    let spawn = gwk_pty_host::engines::spawn_fn(&decl.engine)
        .ok_or_else(|| format!("no adapter claims the engine name {:?}", decl.engine))?;
    registry
        .lock()
        .await
        .spawn(decl.id.clone(), spawn, decl.config())
        .await
        .map_err(|e| e.to_string())
}

/// Kill and reap every session, including request-started sessions.
async fn stop_sessions(registry: &Arc<Mutex<SessionRegistry>>) {
    let mut registry = registry.lock().await;
    for id in registry.ids() {
        // An Err is already-gone — it ended on its own and was reaped, or it
        // never started; neither is a shutdown problem.
        if let Ok(exit) = registry.stop(&id).await {
            tracing::info!(session = %id, ?exit, "session stopped");
        }
    }
    let _ = registry
        .reap()
        .into_iter()
        .map(|(id, exit): (PtySessionId, _)| {
            tracing::info!(session = %id, ?exit, "session reaped");
        })
        .count();
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
