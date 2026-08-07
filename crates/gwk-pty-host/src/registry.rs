//! The session registry: every hosted PTY session by id, and the verbs
//! routed to them — spawn, input, resize, snapshot, attach, stop. Attach
//! reaches the kernel socket today through [`crate::publish`] (a fresh
//! attach carries the snapshot seed, so the kernel's own snapshot verb is
//! served without this registry's ever crossing the wire); the rest stay
//! local until the lifecycle follow-up the crate root doc scopes.
//!
//! Ids are host-minted [`PtySessionId`]s per that type's own contract ("a
//! caller only echoes an id an earlier spawn handed back"); the registry is
//! where minted ids become live sessions.
//!
//! Derivation: none — a map of task handles and channel plumbing; nothing
//! here touches a terminal byte or a process directly (that is
//! [`crate::session`]'s task).

use std::collections::HashMap;
use std::thread::JoinHandle;

use gwk_domain::ids::PtySessionId;
use gwk_pty::SpawnError;
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::session::{
    Attached, DELTA_CHANNEL_CAPACITY, SessionCommand, SessionConfig, SessionExit, Snapshot,
    SpawnFn, run,
};

/// Why a registry verb could not be carried out.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    #[error("a session named {0} already exists")]
    AlreadyExists(PtySessionId),
    #[error("no session named {0}")]
    NotFound(PtySessionId),
    /// The session's task has ended — its child exited, it was stopped, or
    /// it failed. The registry still holds the entry until [`reap`]
    /// (`SessionRegistry::reap`) or [`stop`](SessionRegistry::stop) collects
    /// it, so a caller can distinguish "gone" from "never existed".
    #[error("session {0} has ended")]
    Ended(PtySessionId),
    #[error("could not spawn the engine child: {0}")]
    Spawn(#[from] SpawnError),
    #[error("could not start the session's thread: {0}")]
    Thread(String),
}

/// One live session's handles.
struct Entry {
    commands: mpsc::Sender<SessionCommand>,
    task: JoinHandle<SessionExit>,
}

/// One session's attach handle, detached from the registry — see
/// [`SessionRegistry::attacher`].
#[derive(Clone)]
pub struct Attacher {
    id: PtySessionId,
    commands: mpsc::Sender<SessionCommand>,
}

impl Attacher {
    /// Exactly [`SessionRegistry::attach`], without the registry.
    pub async fn attach(&self, cursor: Option<u64>) -> Result<Attached, RegistryError> {
        let (reply, answer) = oneshot::channel();
        self.commands
            .send(SessionCommand::Attach { cursor, reply })
            .await
            .map_err(|_| RegistryError::Ended(self.id.clone()))?;
        answer
            .await
            .map_err(|_| RegistryError::Ended(self.id.clone()))
    }
}

/// Every hosted session, by id.
#[derive(Default)]
pub struct SessionRegistry {
    sessions: HashMap<PtySessionId, Entry>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a session: spawn the engine child through `spawn` and hand the
    /// running pieces to a supervision task.
    ///
    /// Each session runs on ITS OWN THREAD with a single-threaded runtime,
    /// not on the caller's: the engine's grid and render state hold raw
    /// libghostty-vt pointers and are not `Send`, so they must be born and
    /// die on one thread. The channels are what cross it. The first spawn's
    /// result still comes back to this caller as a typed error — a command
    /// that cannot start (missing binary, refused dimensions) fails HERE,
    /// not at a distance.
    pub async fn spawn(
        &mut self,
        id: PtySessionId,
        spawn: SpawnFn,
        config: SessionConfig,
    ) -> Result<(), RegistryError> {
        if self.sessions.contains_key(&id) {
            return Err(RegistryError::AlreadyExists(id));
        }
        let (commands, receiver) = mpsc::channel(DELTA_CHANNEL_CAPACITY);
        let (deltas_tx, _) = broadcast::channel(DELTA_CHANNEL_CAPACITY);
        let (ready_tx, ready_rx) = oneshot::channel::<Result<(), SpawnError>>();

        let task = std::thread::Builder::new()
            .name(format!("gwk-pty-{id}"))
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(e) => {
                        // The receiver learns of the failure through the
                        // dropped `ready_tx`; the exit records why.
                        return SessionExit::Failed(format!("session runtime: {e}"));
                    }
                };
                runtime.block_on(async move {
                    // Spawned inside the runtime: the PTY registers with the
                    // reactor at creation, so there must be one current.
                    let session = match spawn(config.cols, config.rows) {
                        Ok(session) => {
                            let _ = ready_tx.send(Ok(()));
                            session
                        }
                        Err(e) => {
                            let _ = ready_tx.send(Err(e));
                            return SessionExit::Failed(
                                "the first spawn failed; reported to the caller".to_owned(),
                            );
                        }
                    };
                    run(session, spawn, config, receiver, deltas_tx).await
                })
            })
            .map_err(|e| RegistryError::Thread(e.to_string()))?;

        match ready_rx.await {
            Ok(Ok(())) => {
                self.sessions.insert(id, Entry { commands, task });
                Ok(())
            }
            Ok(Err(spawn_error)) => {
                let _ = task.join();
                Err(RegistryError::Spawn(spawn_error))
            }
            Err(_) => {
                let exit = task
                    .join()
                    .unwrap_or_else(|_| SessionExit::Failed("the thread panicked".to_owned()));
                Err(RegistryError::Thread(format!(
                    "the session thread ended before its first spawn: {exit:?}"
                )))
            }
        }
    }

    /// Send input to a session's child.
    pub async fn input(&self, id: &PtySessionId, bytes: Vec<u8>) -> Result<(), RegistryError> {
        self.send(id, SessionCommand::Input(bytes)).await
    }

    /// Resize a session's grid and PTY.
    pub async fn resize(
        &self,
        id: &PtySessionId,
        cols: u16,
        rows: u16,
    ) -> Result<(), RegistryError> {
        self.send(id, SessionCommand::Resize { cols, rows }).await
    }

    /// The full screen at its current revision.
    pub async fn snapshot(&self, id: &PtySessionId) -> Result<Snapshot, RegistryError> {
        let (reply, answer) = oneshot::channel();
        self.send(id, SessionCommand::Snapshot { reply }).await?;
        answer.await.map_err(|_| RegistryError::Ended(id.clone()))
    }

    /// Attach to a session's live output: `None` for a fresh consumer,
    /// `Some(seq)` to resume after that revision. Detaching is dropping the
    /// returned subscription; reattaching is calling this again with the
    /// last seq the consumer actually received.
    pub async fn attach(
        &self,
        id: &PtySessionId,
        cursor: Option<u64>,
    ) -> Result<Attached, RegistryError> {
        self.attacher(id)?.attach(cursor).await
    }

    /// A standalone attach handle for one session — a clone of its command
    /// channel, usable without the registry or any lock around it. This
    /// exists for the publisher tasks: an attach round-trips through the
    /// session's own thread, and a caller that held a shared registry lock
    /// across that await would stall every other caller on one session's
    /// worst moment (a restart backoff, a child that will not reap). The
    /// handle outlives nothing — once the session's task ends, every verb
    /// on it answers [`RegistryError::Ended`].
    pub fn attacher(&self, id: &PtySessionId) -> Result<Attacher, RegistryError> {
        let entry = self
            .sessions
            .get(id)
            .ok_or_else(|| RegistryError::NotFound(id.clone()))?;
        Ok(Attacher {
            id: id.clone(),
            commands: entry.commands.clone(),
        })
    }

    /// Stop a session — kill its child, reap it, and remove the entry —
    /// returning how the task ended.
    pub async fn stop(&mut self, id: &PtySessionId) -> Result<SessionExit, RegistryError> {
        let entry = self
            .sessions
            .remove(id)
            .ok_or_else(|| RegistryError::NotFound(id.clone()))?;
        // An already-ended task ignores the command; joining still yields
        // its real exit. The join is a blocking thread join, so it moves to
        // the blocking pool rather than stalling the runtime.
        let _ = entry.commands.send(SessionCommand::Stop).await;
        let exit = tokio::task::spawn_blocking(move || {
            entry
                .task
                .join()
                .unwrap_or_else(|_| SessionExit::Failed("the session thread panicked".to_owned()))
        })
        .await
        .unwrap_or_else(|e| SessionExit::Failed(format!("joining the session thread: {e}")));
        Ok(exit)
    }

    /// Remove every session whose task has already ended, returning each
    /// id with its exit. Sessions still running are untouched — the join
    /// only ever runs on a thread `is_finished` already reported done.
    pub fn reap(&mut self) -> Vec<(PtySessionId, SessionExit)> {
        let ended: Vec<PtySessionId> = self
            .sessions
            .iter()
            .filter(|(_, entry)| entry.task.is_finished())
            .map(|(id, _)| id.clone())
            .collect();
        let mut reaped = Vec::with_capacity(ended.len());
        for id in ended {
            if let Some(entry) = self.sessions.remove(&id)
                && let Ok(exit) = entry.task.join()
            {
                reaped.push((id, exit));
            }
        }
        reaped
    }

    /// Every live session id.
    pub fn ids(&self) -> Vec<PtySessionId> {
        self.sessions.keys().cloned().collect()
    }

    async fn send(&self, id: &PtySessionId, command: SessionCommand) -> Result<(), RegistryError> {
        let entry = self
            .sessions
            .get(id)
            .ok_or_else(|| RegistryError::NotFound(id.clone()))?;
        entry
            .commands
            .send(command)
            .await
            .map_err(|_| RegistryError::Ended(id.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{CatchUp, DeltaBatch, RestartPolicy};
    use gwk_domain::frame::{PtyDelta, PtyFrame, StyledCell};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    // Tests spawn /bin/sh, /bin/cat, and /bin/true, never a real engine —
    // the clean-room rule that a test must not depend on (or exercise) an
    // engine binary that happens to be on the box.

    fn cat() -> SpawnFn {
        Box::new(|cols, rows| {
            gwk_pty::Session::spawn(pty_process::Command::new("/bin/cat"), cols, rows)
        })
    }

    fn config() -> SessionConfig {
        SessionConfig {
            cols: 40,
            rows: 6,
            recording_cap: 1024,
            retained_batches: 1024,
            restart: RestartPolicy::Never,
        }
    }

    fn id(name: &str) -> PtySessionId {
        PtySessionId::new(name)
    }

    /// A consumer's copy of the screen, driven only by what the wire carries.
    struct Model {
        cells: Vec<Vec<StyledCell>>,
    }

    impl Model {
        fn from_frame(frame: &PtyFrame) -> Self {
            Self {
                cells: frame.cells.clone(),
            }
        }

        fn apply(&mut self, batch: &DeltaBatch) {
            for delta in batch.deltas.iter() {
                match delta {
                    PtyDelta::CellsChanged { updates } => {
                        for update in updates {
                            self.cells[usize::from(update.row)][usize::from(update.col)] =
                                update.cell.clone();
                        }
                    }
                    PtyDelta::Resized { .. } => {
                        // A consumer re-seeds from a snapshot on resize; the
                        // tests that exercise resize do exactly that.
                    }
                }
            }
        }

        fn row_text(&self, y: usize) -> String {
            self.cells[y]
                .iter()
                .map(|cell| cell.glyph.as_str())
                .collect::<String>()
                .trim_end()
                .to_owned()
        }
    }

    fn frame_row_text(frame: &PtyFrame, y: usize) -> String {
        frame.cells[y]
            .iter()
            .map(|cell| cell.glyph.as_str())
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    /// Poll the session's snapshot until `predicate` holds — bounded, so a
    /// broken session fails the test instead of hanging it.
    async fn await_snapshot(
        registry: &SessionRegistry,
        id: &PtySessionId,
        predicate: impl Fn(&Snapshot) -> bool,
    ) -> Snapshot {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(snapshot) = registry.snapshot(id).await
                    && predicate(&snapshot)
                {
                    return snapshot;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the awaited screen state never arrived")
    }

    #[tokio::test]
    async fn input_reaches_the_child_and_its_echo_reaches_the_snapshot() {
        let mut registry = SessionRegistry::new();
        registry
            .spawn(id("s1"), cat(), config())
            .await
            .expect("spawn");

        registry
            .input(&id("s1"), b"hello\n".to_vec())
            .await
            .expect("input");
        let snapshot = await_snapshot(&registry, &id("s1"), |s| {
            frame_row_text(&s.frame, 0).starts_with("hello")
        })
        .await;
        assert!(snapshot.seq.is_some(), "output must have produced frames");

        let exit = registry.stop(&id("s1")).await.expect("stop");
        assert!(matches!(exit, SessionExit::Stopped), "{exit:?}");
    }

    #[tokio::test]
    async fn detach_and_reattach_resumes_without_a_gap() {
        let mut registry = SessionRegistry::new();
        registry
            .spawn(id("s1"), cat(), config())
            .await
            .expect("spawn");

        // Attach fresh: the catch-up is a snapshot by definition.
        let attached = registry.attach(&id("s1"), None).await.expect("attach");
        let CatchUp::Snapshotted(seed) = attached.catch_up else {
            panic!("a fresh attach must be snapshotted");
        };
        let mut model = Model::from_frame(&seed.frame);
        let mut live = attached.live;

        registry
            .input(&id("s1"), b"one\n".to_vec())
            .await
            .expect("input");
        let cursor = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let batch = live.recv().await.expect("live stream");
                let seq = batch.seq;
                model.apply(&batch);
                if model.row_text(0).starts_with("one") {
                    return seq;
                }
            }
        })
        .await
        .expect("the echo never arrived");

        // Detach IS dropping the subscription.
        drop(live);

        registry
            .input(&id("s1"), b"two\n".to_vec())
            .await
            .expect("input");
        // Quiescence, not first sight: `cat` echoes the line AND writes its
        // own copy (rows 2 and 3). Waiting for both means nothing more is in
        // flight when the comparison snapshot is taken — otherwise the
        // model, which reattaches LATER, could legitimately be ahead of it.
        let current = await_snapshot(&registry, &id("s1"), |s| {
            frame_row_text(&s.frame, 2).starts_with("two")
                && frame_row_text(&s.frame, 3).starts_with("two")
        })
        .await;

        // Reattach from the cursor: the retained batches must cover the gap
        // and land the model on exactly the live screen.
        let attached = registry
            .attach(&id("s1"), Some(cursor))
            .await
            .expect("reattach");
        let CatchUp::Replayed(batches) = attached.catch_up else {
            panic!("a cursor inside retention must be replayed");
        };
        assert!(
            batches.iter().all(|batch| batch.seq > cursor),
            "replay must start strictly after the cursor"
        );
        for batch in &batches {
            model.apply(batch);
        }
        assert_eq!(
            model.cells, current.frame.cells,
            "the replayed model diverged from the live screen"
        );

        registry.stop(&id("s1")).await.expect("stop");
    }

    #[tokio::test]
    async fn a_cursor_older_than_retention_is_snapshotted_instead() {
        let mut registry = SessionRegistry::new();
        let mut tight = config();
        tight.retained_batches = 1;
        registry.spawn(id("s1"), cat(), tight).await.expect("spawn");

        // Distinct batches, forced: each line is awaited on screen before
        // the next is sent, so the echoes cannot coalesce into one read —
        // back-to-back writes can land in a single chunk, which would leave
        // seq 0 the only evicted batch and a cursor of 0 legitimately
        // servable by replay.
        for (line, marker) in [
            (&b"aa\n"[..], "aa"),
            (b"bb\n", "bb"),
            (b"cc\n", "cc"),
            (b"dd\n", "dd"),
        ] {
            registry
                .input(&id("s1"), line.to_vec())
                .await
                .expect("input");
            await_snapshot(&registry, &id("s1"), |s| {
                (0..s.frame.cells.len()).any(|y| frame_row_text(&s.frame, y).starts_with(marker))
            })
            .await;
        }

        let attached = registry.attach(&id("s1"), Some(0)).await.expect("reattach");
        assert!(
            matches!(attached.catch_up, CatchUp::Snapshotted(_)),
            "a cursor behind the eviction horizon must fall back to a snapshot"
        );

        registry.stop(&id("s1")).await.expect("stop");
    }

    #[tokio::test]
    async fn a_failed_child_restarts_up_to_the_cap_and_the_exit_is_reported() {
        let spawned = Arc::new(AtomicU32::new(0));
        let counter = spawned.clone();
        let factory: SpawnFn = Box::new(move |cols, rows| {
            counter.fetch_add(1, Ordering::SeqCst);
            gwk_pty::Session::spawn(
                pty_process::Command::new("/bin/sh").arg("-c").arg("exit 3"),
                cols,
                rows,
            )
        });
        let mut registry = SessionRegistry::new();
        let mut config = config();
        config.restart = RestartPolicy::OnFailure {
            max: 2,
            delay: Duration::from_millis(10),
        };
        registry
            .spawn(id("s1"), factory, config)
            .await
            .expect("spawn");

        let reaped = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let reaped = registry.reap();
                if !reaped.is_empty() {
                    return reaped;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the failing session never ended");

        assert_eq!(
            spawned.load(Ordering::SeqCst),
            3,
            "one initial spawn plus exactly two restarts"
        );
        let (_, exit) = &reaped[0];
        let SessionExit::Exited(status) = exit else {
            panic!("expected the child's own exit, got {exit:?}");
        };
        assert!(!status.success(), "the child exits 3 every time");
    }

    #[tokio::test]
    async fn a_clean_exit_is_not_restarted() {
        let spawned = Arc::new(AtomicU32::new(0));
        let counter = spawned.clone();
        let factory: SpawnFn = Box::new(move |cols, rows| {
            counter.fetch_add(1, Ordering::SeqCst);
            gwk_pty::Session::spawn(pty_process::Command::new("/bin/true"), cols, rows)
        });
        let mut registry = SessionRegistry::new();
        let mut config = config();
        config.restart = RestartPolicy::OnFailure {
            max: 5,
            delay: Duration::from_millis(10),
        };
        registry
            .spawn(id("s1"), factory, config)
            .await
            .expect("spawn");

        tokio::time::timeout(Duration::from_secs(10), async {
            while registry.reap().is_empty() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the session never ended");
        assert_eq!(
            spawned.load(Ordering::SeqCst),
            1,
            "a clean exit must not trip the restart policy"
        );
    }

    #[tokio::test]
    async fn a_resize_reaches_consumers_and_the_snapshot_dimensions_move() {
        let mut registry = SessionRegistry::new();
        registry
            .spawn(id("s1"), cat(), config())
            .await
            .expect("spawn");
        let attached = registry.attach(&id("s1"), None).await.expect("attach");
        let mut live = attached.live;

        registry.resize(&id("s1"), 100, 30).await.expect("resize");

        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let batch = live.recv().await.expect("live stream");
                if batch.deltas.iter().any(|d| {
                    matches!(
                        d,
                        PtyDelta::Resized {
                            rows: 30,
                            cols: 100
                        }
                    )
                }) {
                    return;
                }
            }
        })
        .await
        .expect("the resize never reached the consumer");

        let snapshot = registry.snapshot(&id("s1")).await.expect("snapshot");
        assert_eq!(snapshot.frame.cells.len(), 30);
        assert!(snapshot.frame.cells.iter().all(|row| row.len() == 100));

        registry.stop(&id("s1")).await.expect("stop");
    }

    #[tokio::test]
    async fn registry_verbs_answer_typed_for_unknown_duplicate_and_unspawnable() {
        let mut registry = SessionRegistry::new();

        assert!(matches!(
            registry.input(&id("ghost"), b"x".to_vec()).await,
            Err(RegistryError::NotFound(_))
        ));

        registry
            .spawn(id("s1"), cat(), config())
            .await
            .expect("spawn");
        assert!(matches!(
            registry.spawn(id("s1"), cat(), config()).await,
            Err(RegistryError::AlreadyExists(_))
        ));

        let missing: SpawnFn = Box::new(|cols, rows| {
            gwk_pty::Session::spawn(
                pty_process::Command::new("/nonexistent-gwk-engine"),
                cols,
                rows,
            )
        });
        assert!(matches!(
            registry.spawn(id("s2"), missing, config()).await,
            Err(RegistryError::Spawn(_))
        ));

        registry.stop(&id("s1")).await.expect("stop");
    }
}
