//! The `PreToolUse` relay: a Unix-domain socket bridging the hook subprocess
//! Claude Code invokes to the host process that actually decides.
//!
//! No network listener of any kind — a peer is checked against this
//! process's own effective uid before a single byte of its message is
//! parsed, mirroring the trust pattern `gwk-kernel`'s own Unix-socket
//! listener holds itself to (private 0700 directory, 0600 socket, peer-uid
//! check on accept, an unauthorized peer dropped without a word rather than
//! answered) — an internal convention this crate matches on purpose, not a
//! `CLAUDE-*` behavior, so nothing in that sentence carries a
//! `Derivation:` marker. And a client that cannot reach the server, or does
//! not hear back inside its deadline, gets `deny` — never a hang and never
//! `allow`: a relay that fails open is an authority bypass. Deciding stays
//! kernel-side; this module only transports the ask and the decision.
// Derivation: CLAUDE-HOOKS — the reason this relay exists at all: a
// `PreToolUse` invocation is a fresh, short-lived process whose only
// decision-returning path is its OWN stdout ("Exit 0 with JSON for
// structured control"), so a decision computed elsewhere in the host has no
// way back into that specific invocation except by being handed to it
// before that process's stdout is written — which is exactly what
// `RelayResponder::respond` does.

use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::hook::{HookDecision, PermissionDecision, PreToolUseAsk};
use crate::io_util::{read_bounded_line_async, read_bounded_line_sync};

/// Mode of the private directory the socket must live in.
pub const RELAY_DIR_MODE: u32 = 0o700;
/// Mode of the socket file itself.
pub const RELAY_SOCKET_MODE: u32 = 0o600;
/// One relay line, either direction — bounded so a runaway peer on either
/// end cannot grow an unbounded buffer.
pub const MAX_RELAY_LINE_BYTES: usize = 256 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum RelayServerError {
    #[error("{0}")]
    Config(String),
    #[error("peer credentials unavailable or foreign: {0}")]
    Privilege(String),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error("malformed relay response: {0}")]
    Malformed(String),
}

/// A bound relay socket. One `bind()` per engine session; every `PreToolUse`
/// invocation for that session connects fresh to the same path.
#[derive(Debug)]
pub struct AskRelay {
    listener: UnixListener,
    path: PathBuf,
}

/// The still-open connection an accepted ask arrived on — held only long
/// enough to answer it.
pub struct RelayResponder {
    io: BufReader<UnixStream>,
}

impl AskRelay {
    /// Bind `path`, requiring its parent directory to already be a private
    /// (0700, ours) real directory, and requiring `path` itself to not
    /// already exist — the relay socket is session-scoped and freshly
    /// minted, never a stale file to take over.
    pub async fn bind(path: &Path) -> Result<Self, RelayServerError> {
        check_private_parent(path)?;
        if std::fs::symlink_metadata(path).is_ok() {
            return Err(RelayServerError::Config(format!(
                "{} already exists; the relay socket must be freshly minted per session",
                path.display()
            )));
        }
        let listener = UnixListener::bind(path)
            .map_err(|e| RelayServerError::Config(format!("bind {}: {e}", path.display())))?;
        // Mode AFTER bind: bind creates the file under the process umask, and
        // the private parent directory (checked above) closes the window
        // before this chmod lands.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(RELAY_SOCKET_MODE))
            .map_err(|e| RelayServerError::Config(format!("chmod {}: {e}", path.display())))?;
        let mode = std::fs::symlink_metadata(path)
            .map_err(|e| RelayServerError::Config(format!("stat {}: {e}", path.display())))?
            .permissions()
            .mode()
            & 0o777;
        if mode != RELAY_SOCKET_MODE {
            return Err(RelayServerError::Config(format!(
                "{} is mode {mode:04o} after bind, not {RELAY_SOCKET_MODE:04o}",
                path.display()
            )));
        }
        Ok(Self {
            listener,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Accept one ask, refusing any peer that is not this process's own
    /// effective uid, and any connection whose request does not parse. Both
    /// refusals are dropped without a word — like `gwk-kernel`'s listener —
    /// so no unauthorized or malformed byte ever reaches the caller.
    pub async fn accept(&self) -> Result<(PreToolUseAsk, RelayResponder), RelayServerError> {
        loop {
            let (stream, _) = self.listener.accept().await?;
            if authorized(&stream).is_err() {
                continue;
            }
            let mut io = BufReader::new(stream);
            let Ok(Some(line)) = read_bounded_line_async(&mut io, MAX_RELAY_LINE_BYTES).await
            else {
                continue;
            };
            let Ok(ask) = serde_json::from_slice::<PreToolUseAsk>(&line) else {
                continue;
            };
            return Ok((ask, RelayResponder { io }));
        }
    }
}

impl Drop for AskRelay {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl RelayResponder {
    /// Send the decision back over the same connection the ask arrived on —
    /// the requirement axis 4 states plainly: "the decision returns to the
    /// engine through the same channel that raised it."
    pub async fn respond(
        mut self,
        decision: PermissionDecision,
        reason: Option<String>,
    ) -> Result<(), RelayServerError> {
        let response = HookDecision { decision, reason };
        let mut line = serde_json::to_vec(&response)
            .map_err(|e| RelayServerError::Malformed(e.to_string()))?;
        line.push(b'\n');
        self.io.write_all(&line).await?;
        self.io.flush().await?;
        Ok(())
    }
}

fn authorized(stream: &UnixStream) -> Result<(), RelayServerError> {
    let cred = stream
        .peer_cred()
        .map_err(|e| RelayServerError::Privilege(format!("peer credentials unavailable: {e}")))?;
    let ours = rustix::process::geteuid().as_raw();
    if cred.uid() != ours {
        return Err(RelayServerError::Privilege(format!(
            "peer uid {} is not the relay's {ours}",
            cred.uid()
        )));
    }
    Ok(())
}

fn check_private_parent(path: &Path) -> Result<(), RelayServerError> {
    let parent = path.parent().ok_or_else(|| {
        RelayServerError::Config(format!("{} has no parent directory", path.display()))
    })?;
    let stat = std::fs::symlink_metadata(parent)
        .map_err(|e| RelayServerError::Config(format!("stat {}: {e}", parent.display())))?;
    if stat.file_type().is_symlink() || !stat.is_dir() {
        return Err(RelayServerError::Config(format!(
            "{} must be a real directory, not a symlink",
            parent.display()
        )));
    }
    let mode = stat.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(RelayServerError::Config(format!(
            "{} is mode {mode:04o}; the relay directory must be {RELAY_DIR_MODE:04o}",
            parent.display()
        )));
    }
    if stat.uid() != rustix::process::geteuid().as_raw() {
        return Err(RelayServerError::Config(format!(
            "{} is owned by uid {}, not ours",
            parent.display(),
            stat.uid()
        )));
    }
    Ok(())
}

// ---- the hook-side blocking client ----

#[derive(Debug, thiserror::Error)]
pub enum RelayClientError {
    #[error("connect {path}: {source}")]
    Connect {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("deadline exceeded before the relay answered")]
    DeadlineExceeded,
    #[error("io error talking to the relay: {0}")]
    Io(#[from] io::Error),
    #[error("could not encode the ask: {0}")]
    Encode(serde_json::Error),
    #[error("malformed relay response: {0}")]
    Decode(serde_json::Error),
    #[error("the relay closed the connection without answering")]
    ConnectionClosed,
}

/// Ask the relay and wait up to `deadline` for a decision.
///
/// This function NEVER returns an error: every failure mode — connect
/// refused, deadline exceeded, a malformed reply, the relay hanging up —
/// becomes `PermissionDecision::Deny`. `reason` says why, for the engine to
/// show; the decision itself is never anything other than what the relay
/// explicitly returned, or a fail-closed deny. This is deliberately a
/// plain blocking function on `std::os::unix::net` — the hook subprocess
/// Claude Code spawns per tool call is not, and should not need to be, a
/// Tokio runtime.
pub fn relay_ask(socket_path: &Path, ask: &PreToolUseAsk, deadline: Duration) -> HookDecision {
    match try_relay_ask(socket_path, ask, deadline) {
        Ok(decision) => decision,
        Err(e) => HookDecision {
            decision: PermissionDecision::Deny,
            reason: Some(format!("PreToolUse relay failed closed: {e}")),
        },
    }
}

fn try_relay_ask(
    socket_path: &Path,
    ask: &PreToolUseAsk,
    deadline: Duration,
) -> Result<HookDecision, RelayClientError> {
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    let start = Instant::now();
    let stream = UnixStream::connect(socket_path).map_err(|e| RelayClientError::Connect {
        path: socket_path.to_path_buf(),
        source: e,
    })?;

    let remaining = |start: Instant| -> Result<Duration, RelayClientError> {
        deadline
            .checked_sub(start.elapsed())
            .ok_or(RelayClientError::DeadlineExceeded)
    };

    stream.set_write_timeout(Some(remaining(start)?.max(Duration::from_millis(1))))?;
    let mut line = serde_json::to_vec(ask).map_err(RelayClientError::Encode)?;
    line.push(b'\n');
    (&stream).write_all(&line)?;

    stream.set_read_timeout(Some(remaining(start)?.max(Duration::from_millis(1))))?;
    let mut reader = std::io::BufReader::new(&stream);
    let raw = read_bounded_line_sync(&mut reader, MAX_RELAY_LINE_BYTES)?
        .ok_or(RelayClientError::ConnectionClosed)?;
    serde_json::from_slice(&raw).map_err(RelayClientError::Decode)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn private_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "gwk-adapter-claude-relay-{}-{tag}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(RELAY_DIR_MODE))
            .expect("chmod test dir");
        dir
    }

    fn sample_ask() -> PreToolUseAsk {
        PreToolUseAsk::from_stdin(
            br#"{
                "session_id": "sess-1",
                "transcript_path": "/tmp/t.json",
                "cwd": "/repo",
                "hook_event_name": "PreToolUse",
                "tool_name": "Bash",
                "tool_input": {"command": "echo hi"},
                "tool_use_id": "tool-1"
            }"#,
        )
        .expect("decode fixture ask")
    }

    #[tokio::test]
    async fn a_reachable_directory_is_refused() {
        let dir = private_dir("reachable");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
            .expect("widen test dir");
        let error = AskRelay::bind(&dir.join("relay.sock"))
            .await
            .expect_err("world-readable parent must be refused");
        assert!(matches!(error, RelayServerError::Config(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn allow_and_deny_both_round_trip_over_the_socket() {
        for expected in [PermissionDecision::Allow, PermissionDecision::Deny] {
            let dir = private_dir(expected.as_str());
            let socket_path = dir.join("relay.sock");
            let relay = AskRelay::bind(&socket_path).await.expect("bind");

            let ask = sample_ask();
            let client_ask = ask.clone();
            let client_path = socket_path.clone();
            let client = std::thread::spawn(move || {
                relay_ask(&client_path, &client_ask, Duration::from_secs(5))
            });

            let (received, responder) = relay.accept().await.expect("accept");
            assert_eq!(received, ask);
            responder
                .respond(expected, Some("test reason".to_owned()))
                .await
                .expect("respond");

            let decision = client.join().expect("client thread joined");
            assert_eq!(decision.decision, expected);
            assert_eq!(decision.reason, Some("test reason".to_owned()));

            drop(relay);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    #[tokio::test]
    async fn a_client_that_gets_no_response_before_the_deadline_fails_closed_to_deny() {
        let dir = private_dir("deadline");
        let socket_path = dir.join("relay.sock");
        let relay = AskRelay::bind(&socket_path).await.expect("bind");

        let ask = sample_ask();
        let client_path = socket_path.clone();
        let client =
            std::thread::spawn(move || relay_ask(&client_path, &ask, Duration::from_millis(50)));

        // Accept the connection but never respond — the client's own
        // deadline is what must save it, not a server-side answer.
        let (_received, responder) = relay.accept().await.expect("accept");
        let decision = client.join().expect("client thread joined");
        assert_eq!(decision.decision, PermissionDecision::Deny);
        assert!(
            decision
                .reason
                .expect("a reason was recorded")
                .contains("failed closed")
        );

        drop(responder);
        drop(relay);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_client_with_nobody_listening_fails_closed_to_deny() {
        let dir = private_dir("no-listener");
        let socket_path = dir.join("relay.sock");
        let ask = sample_ask();
        let decision = relay_ask(&socket_path, &ask, Duration::from_secs(1));
        assert_eq!(decision.decision, PermissionDecision::Deny);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn binding_the_same_path_twice_is_refused_rather_than_stolen() {
        let dir = private_dir("double-bind");
        let socket_path = dir.join("relay.sock");
        let first = AskRelay::bind(&socket_path).await.expect("first bind");
        let error = AskRelay::bind(&socket_path)
            .await
            .expect_err("a second bind over a live relay must be refused");
        assert!(matches!(error, RelayServerError::Config(_)));
        drop(first);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn dropping_the_relay_removes_its_socket_file() {
        let dir = private_dir("cleanup");
        let socket_path = dir.join("relay.sock");
        let relay = AskRelay::bind(&socket_path).await.expect("bind");
        assert!(socket_path.exists());
        drop(relay);
        assert!(!socket_path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
