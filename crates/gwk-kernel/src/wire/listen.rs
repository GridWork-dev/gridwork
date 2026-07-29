//! The Unix-domain socket: where it may live, who may talk to it, and what is
//! removed when the daemon stops.
//!
//! There is no network path here and there is no code that could grow one — the
//! only listener type in this module is [`UnixListener`] (ADR 0002). The peer
//! credential is the only identity this kernel has, so it is checked before a
//! single byte of the connection is read, and a peer whose credentials the OS
//! declines to report is refused rather than assumed local.
//!
//! Most of this file is about a path that might not be ours. A socket file is
//! the one piece of a daemon's state that outlives it, and the failure modes are
//! all quiet ones: binding over a live daemon's socket steals its clients,
//! removing a stranger's socket breaks a service nobody was asked about, and
//! removing "our" socket on shutdown after someone else replaced it deletes
//! theirs. Every check below exists for one of those three.

use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tokio::net::{UnixListener, UnixStream};

use crate::error::{KernelError, Result};

/// Mode of the directory the socket lives in. Nobody but the owner may reach
/// into it — a group-writable parent lets someone else unlink the socket and
/// bind their own in its place, which no permission on the socket itself
/// prevents.
pub const RUNTIME_DIR_MODE: u32 = 0o700;

/// Mode of the socket itself.
pub const SOCKET_MODE: u32 = 0o600;

/// A bound socket, and the identity of the file it created.
///
/// `device`/`inode` are the whole reason this is a type rather than a bare
/// listener: on shutdown they say whether the path still refers to the file
/// this process made, which is the difference between cleaning up and deleting
/// a successor's socket.
#[derive(Debug)]
pub struct Listener {
    listener: UnixListener,
    path: PathBuf,
    device: u64,
    inode: u64,
}

/// Who is on the other end. `uid` has been checked against this process's
/// effective uid before this exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peer {
    pub uid: u32,
    pub pid: Option<i32>,
}

impl Listener {
    /// Validate the path, take over a stale socket if there is one, bind, and
    /// verify what was bound.
    pub async fn bind(path: &Path) -> Result<Self> {
        check_parent(path)?;
        clear_stale(path).await?;

        let listener = UnixListener::bind(path)
            .map_err(|e| KernelError::Config(format!("bind {}: {e}", path.display())))?;

        // Mode AFTER bind, because bind creates the file with the process umask
        // and a umask of 0 would otherwise leave it world-writable for the
        // window between the two calls. The window is real but it is not
        // reachable: the parent directory is 0700, so nobody else can open the
        // path during it.
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(SOCKET_MODE))
            .map_err(|e| KernelError::Config(format!("chmod {}: {e}", path.display())))?;

        // Read back what is actually there rather than trusting what was just
        // asked for: this is the record that shutdown compares against, so it
        // has to describe the file on disk.
        let stat = std::fs::symlink_metadata(path)
            .map_err(|e| KernelError::Config(format!("stat {}: {e}", path.display())))?;
        let mode = stat.permissions().mode() & 0o777;
        if mode != SOCKET_MODE {
            return Err(KernelError::Config(format!(
                "{} is mode {mode:04o} after bind, not {SOCKET_MODE:04o}",
                path.display()
            )));
        }
        Ok(Self {
            listener,
            path: path.to_path_buf(),
            device: stat.dev(),
            inode: stat.ino(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Accept one connection, refusing any peer that is not this process's own
    /// effective uid.
    ///
    /// The refusal happens here, before the stream reaches the wire layer, so
    /// there is no path on which an unauthorized peer's bytes are parsed at all.
    /// A stream whose credentials cannot be read is refused the same way —
    /// failing closed, because "the OS would not say" is not evidence of
    /// anything.
    pub async fn accept(&self) -> Result<(UnixStream, Peer)> {
        loop {
            let (stream, _) = self
                .listener
                .accept()
                .await
                .map_err(|e| KernelError::Config(format!("accept: {e}")))?;
            match authorized(&stream) {
                Ok(peer) => return Ok((stream, peer)),
                // Dropped without a word: an unauthorized peer gets no frame to
                // parse and no error to distinguish from a closed socket.
                Err(_) => continue,
            }
        }
    }

    /// Remove the socket, but only if the path still names the file this
    /// process created.
    ///
    /// A daemon that unlinked by path alone would, after a crash-and-restart
    /// race, delete the NEW daemon's socket on its way out and leave a running
    /// process nobody can reach.
    pub fn remove(&self) {
        match std::fs::symlink_metadata(&self.path) {
            Ok(stat) if stat.dev() == self.device && stat.ino() == self.inode => {
                let _ = std::fs::remove_file(&self.path);
            }
            // Either gone already or somebody else's now. Both mean there is
            // nothing here this process should delete.
            _ => {}
        }
    }
}

/// The peer's credentials, if it may talk to us at all.
fn authorized(stream: &UnixStream) -> Result<Peer> {
    let cred = stream
        .peer_cred()
        .map_err(|e| KernelError::Privilege(format!("peer credentials unavailable: {e}")))?;
    let ours = daemon_uid();
    if cred.uid() != ours {
        return Err(KernelError::Privilege(format!(
            "peer uid {} is not the daemon's {ours}",
            cred.uid()
        )));
    }
    Ok(Peer {
        uid: cred.uid(),
        pid: cred.pid(),
    })
}

/// This process's EFFECTIVE uid — the one the contract names.
///
/// Effective and not real: a daemon that dropped privileges has a real uid that
/// no longer says what it may do, and file ownership follows the effective one,
/// so comparing against the real uid would disagree with the socket this
/// process actually created.
fn daemon_uid() -> u32 {
    rustix::process::geteuid().as_raw()
}

/// The directory the socket goes in must be a real directory, ours, and
/// unreachable by anyone else.
fn check_parent(path: &Path) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        KernelError::Config(format!("{} has no parent directory", path.display()))
    })?;
    let stat = std::fs::symlink_metadata(parent)
        .map_err(|e| KernelError::Config(format!("stat {}: {e}", parent.display())))?;
    if stat.file_type().is_symlink() {
        // Refused rather than resolved: a symlinked runtime directory means the
        // checks below describe one place and the bind happens in another.
        return Err(KernelError::Config(format!(
            "{} is a symlink, not a directory",
            parent.display()
        )));
    }
    if !stat.is_dir() {
        return Err(KernelError::Config(format!(
            "{} is not a directory",
            parent.display()
        )));
    }
    if stat.uid() != daemon_uid() {
        return Err(KernelError::Config(format!(
            "{} is owned by uid {}, not the daemon's {}",
            parent.display(),
            stat.uid(),
            daemon_uid()
        )));
    }
    let mode = stat.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(KernelError::Config(format!(
            "{} is mode {mode:04o}; the runtime directory must be {RUNTIME_DIR_MODE:04o}",
            parent.display()
        )));
    }
    Ok(())
}

/// Decide what to do about a path that already exists.
///
/// Live socket → refuse, there is a daemon there. Stale socket that is ours →
/// remove it. Anything else — a symlink, a regular file, a stranger's socket —
/// refuse, because the operator asked for a socket at a path that holds
/// something else and guessing which of the two is wrong is not this process's
/// call.
async fn clear_stale(path: &Path) -> Result<()> {
    let before = match std::fs::symlink_metadata(path) {
        Ok(stat) => stat,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(KernelError::Config(format!("stat {}: {e}", path.display()))),
    };
    if before.file_type().is_symlink() {
        return Err(KernelError::Config(format!(
            "{} is a symlink; refusing to bind through it",
            path.display()
        )));
    }
    if !before.file_type().is_socket() {
        return Err(KernelError::Config(format!(
            "{} exists and is not a socket",
            path.display()
        )));
    }
    if before.uid() != daemon_uid() {
        return Err(KernelError::Config(format!(
            "{} is owned by uid {}, not the daemon's {}",
            path.display(),
            before.uid(),
            daemon_uid()
        )));
    }

    // The probe: a socket that accepts is a daemon that is running. Only a
    // refused connection means the file outlived its process.
    match UnixStream::connect(path).await {
        Ok(_) => {
            return Err(KernelError::Config(format!(
                "{} is live; another daemon holds it",
                path.display()
            )));
        }
        Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => {}
        Err(e) => {
            return Err(KernelError::Config(format!(
                "probe {}: {e}",
                path.display()
            )));
        }
    }

    // Re-stat after the probe and require the SAME file. Between the first stat
    // and here another process could have removed this socket and bound its
    // own; unlinking on the strength of the first stat would then delete a live
    // daemon's socket on the evidence of a dead one.
    let after = std::fs::symlink_metadata(path)
        .map_err(|e| KernelError::Config(format!("re-stat {}: {e}", path.display())))?;
    if after.dev() != before.dev() || after.ino() != before.ino() {
        return Err(KernelError::Config(format!(
            "{} changed identity while being probed; refusing to remove it",
            path.display()
        )));
    }
    std::fs::remove_file(path)
        .map_err(|e| KernelError::Config(format!("remove stale {}: {e}", path.display())))
}

/// Tell systemd the daemon is ready, if it is running under systemd.
///
/// Returns whether anything was sent. `NOTIFY_SOCKET` unset is the ordinary
/// case — a daemon started by hand is not being watched — so it is not an
/// error. A path starting with `@` is the abstract namespace, which is spelled
/// with a leading NUL byte at the socket layer.
//
// ponytail: eight lines of datagram instead of a crate. `sd_notify` is one
// message on one socket; the dependency would be larger than the code.
pub fn notify_ready() -> bool {
    let Ok(target) = std::env::var("NOTIFY_SOCKET") else {
        return false;
    };
    if target.is_empty() {
        return false;
    }
    let address = match target.strip_prefix('@') {
        // The abstract namespace exists only on Linux, and so does systemd —
        // an `@` path on any other platform is something this daemon was not
        // started by, so it is left alone rather than reinterpreted as a file.
        #[cfg(target_os = "linux")]
        Some(abstract_name) => {
            use std::os::linux::net::SocketAddrExt;
            match std::os::unix::net::SocketAddr::from_abstract_name(abstract_name.as_bytes()) {
                Ok(address) => address,
                Err(_) => return false,
            }
        }
        #[cfg(not(target_os = "linux"))]
        Some(_) => return false,
        None => match std::os::unix::net::SocketAddr::from_pathname(&target) {
            Ok(address) => address,
            Err(_) => return false,
        },
    };
    let Ok(socket) = std::os::unix::net::UnixDatagram::unbound() else {
        return false;
    };
    socket.send_to_addr(b"READY=1\n", &address).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A private runtime directory, as the daemon requires.
    fn runtime_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gwk-sock-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create runtime dir");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(RUNTIME_DIR_MODE))
            .expect("chmod runtime dir");
        dir
    }

    #[tokio::test]
    async fn a_bound_socket_is_private_and_knows_its_own_file() {
        let dir = runtime_dir("bind");
        let path = dir.join("gwk.sock");
        let listener = Listener::bind(&path).await.expect("bind");

        let stat = std::fs::symlink_metadata(&path).expect("stat");
        assert_eq!(stat.permissions().mode() & 0o777, SOCKET_MODE);
        assert!(stat.file_type().is_socket());
        assert_eq!((stat.dev(), stat.ino()), (listener.device, listener.inode));

        listener.remove();
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_live_socket_is_never_taken_over() {
        // The case that matters most: binding over a running daemon steals
        // every client that connects afterwards, silently.
        let dir = runtime_dir("live");
        let path = dir.join("gwk.sock");
        let held = Listener::bind(&path).await.expect("first bind");

        let error = Listener::bind(&path)
            .await
            .expect_err("bound over a live socket");
        assert!(error.to_string().contains("live"), "{error}");

        held.remove();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_stale_socket_is_probed_and_replaced() {
        let dir = runtime_dir("stale");
        let path = dir.join("gwk.sock");
        // Bind and drop WITHOUT removing: the file survives the listener, which
        // is exactly what a kill -9 leaves behind.
        {
            let orphan = Listener::bind(&path).await.expect("first bind");
            std::mem::forget(orphan);
        }
        // Close the fd the forgotten listener held by rebinding in a child
        // scope is not possible, so drop it properly and re-create the file as
        // a dead socket instead.
        let _ = std::fs::remove_file(&path);
        {
            let dying = std::os::unix::net::UnixListener::bind(&path).expect("dead socket");
            drop(dying);
        }
        assert!(path.exists(), "the stale file should still be there");

        let fresh = Listener::bind(&path)
            .await
            .expect("take over the stale socket");
        assert!(path.exists());
        fresh.remove();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_path_that_is_not_a_socket_is_refused_rather_than_replaced() {
        let dir = runtime_dir("notsock");
        let path = dir.join("gwk.sock");
        std::fs::write(&path, b"not a socket").expect("write");
        let error = Listener::bind(&path)
            .await
            .expect_err("bound over a regular file");
        assert!(error.to_string().contains("not a socket"), "{error}");
        // Still there: refusing means refusing, not deleting what it found.
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_symlinked_path_is_refused_without_following_it() {
        let dir = runtime_dir("symlink");
        let real = dir.join("elsewhere.sock");
        let path = dir.join("gwk.sock");
        std::os::unix::fs::symlink(&real, &path).expect("symlink");
        let error = Listener::bind(&path)
            .await
            .expect_err("bound through a symlink");
        assert!(error.to_string().contains("symlink"), "{error}");
        assert!(!real.exists(), "the target was created through the link");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_reachable_runtime_directory_is_refused() {
        let dir = runtime_dir("mode");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755))
            .expect("widen the directory");
        let error = Listener::bind(&dir.join("gwk.sock"))
            .await
            .expect_err("bound in a world-readable directory");
        assert!(error.to_string().contains("0755"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn shutdown_removes_only_the_file_this_process_created() {
        // The crash-and-restart race: the old daemon's cleanup must not delete
        // the new daemon's socket.
        let dir = runtime_dir("succession");
        let path = dir.join("gwk.sock");
        let old = Listener::bind(&path).await.expect("first bind");
        std::fs::remove_file(&path).expect("simulate the old socket going away");
        let new = std::os::unix::net::UnixListener::bind(&path).expect("successor binds");

        old.remove();
        assert!(
            path.exists(),
            "the old listener deleted its successor's socket"
        );

        drop(new);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_local_peer_is_accepted_and_reports_our_own_uid() {
        let dir = runtime_dir("peer");
        let path = dir.join("gwk.sock");
        let listener = Listener::bind(&path).await.expect("bind");

        let connecting = tokio::spawn({
            let path = path.clone();
            async move { UnixStream::connect(&path).await.expect("connect") }
        });
        let (_, peer) = listener.accept().await.expect("accept");
        let client = connecting.await.expect("join");

        // Same process, so the same uid — which is the check passing, not it
        // being vacuous: `authorized` compares against the effective uid and a
        // different one is the branch a foreign peer takes.
        assert_eq!(peer.uid, daemon_uid());
        assert_eq!(peer.pid, Some(std::process::id() as i32));

        drop(client);
        listener.remove();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_notify_socket_is_not_a_failure() {
        // The ordinary case for a daemon started by hand. `notify_ready` says
        // "nothing sent", and the caller must not treat that as an error.
        // SAFETY-adjacent: this test reads the ambient environment and does not
        // set it, so it cannot race another test that does.
        if std::env::var_os("NOTIFY_SOCKET").is_none() {
            assert!(!notify_ready());
        }
    }
}
