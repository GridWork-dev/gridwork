//! The session registry: every hosted PTY session by id, and the verbs
//! routed to them — spawn, input, resize, snapshot, attach, stop. Attach
//! reaches the kernel socket today through [`crate::publish`] (a fresh
//! attach carries the snapshot seed, so the kernel's own snapshot verb is
//! served without this registry's ever crossing the wire); input, resize, and
//! stop also arrive through the publisher connection's bounded control queue.
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
use std::time::Duration;

use gwk_domain::ids::{EventId, PtySessionId};
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
    #[error("input write to session {session_id} failed: {message}")]
    Input {
        session_id: PtySessionId,
        message: String,
    },
    #[error("input write to session {0} exceeded the delivery deadline")]
    InputTimeout(PtySessionId),
    #[error("control delivery to session {0} exceeded the delivery deadline")]
    ControlTimeout(PtySessionId),
    #[error("application by session {0} was not confirmed before the delivery deadline")]
    ApplicationIndeterminate(PtySessionId),
    #[error("the host already holds {0} PTY sessions")]
    Overloaded(usize),
    #[error("the PTY delivery deduplication window is full of unsettled commands")]
    DedupCapacity,
}

/// One live session's handles.
struct Entry {
    commands: mpsc::Sender<SessionCommand>,
    task: JoinHandle<SessionExit>,
    delivered: std::sync::Arc<tokio::sync::Mutex<DeliveredCommands>>,
}

/// One session's attach handle, detached from the registry — see
/// [`SessionRegistry::attacher`].
#[derive(Debug, Clone)]
pub struct Attacher {
    id: PtySessionId,
    commands: mpsc::Sender<SessionCommand>,
    delivered: std::sync::Arc<tokio::sync::Mutex<DeliveredCommands>>,
}

const DELIVERED_COMMANDS: usize = 1_024;

#[derive(Debug, Default)]
struct DeliveredCommands {
    order: std::collections::VecDeque<EventId>,
    ids: std::collections::HashSet<EventId>,
}

impl DeliveredCommands {
    fn contains(&self, id: &EventId) -> bool {
        self.ids.contains(id)
    }

    fn insert(&mut self, id: EventId) {
        if !self.ids.insert(id.clone()) {
            return;
        }
        self.order.push_back(id);
    }

    fn ids(&self) -> Vec<EventId> {
        self.order.iter().cloned().collect()
    }

    fn settle(&mut self, id: &EventId) {
        if self.ids.remove(id) {
            self.order.retain(|candidate| candidate != id);
        }
    }
}

impl Attacher {
    pub fn id(&self) -> &PtySessionId {
        &self.id
    }

    /// Send one raw input batch without reacquiring the registry lock.
    pub async fn input(&self, bytes: Vec<u8>) -> Result<(), RegistryError> {
        let id = self.id.clone();
        let (reply, answer) = oneshot::channel();
        tokio::time::timeout(
            Duration::from_secs(gwk_domain::protocol::SLOW_CONSUMER_TIMEOUT_SECS),
            self.commands.send(SessionCommand::Input { bytes, reply }),
        )
        .await
        .map_err(|_| RegistryError::InputTimeout(self.id.clone()))?
        .map_err(|_| RegistryError::Ended(id.clone()))?;
        match tokio::time::timeout(
            Duration::from_secs(gwk_domain::protocol::SLOW_CONSUMER_TIMEOUT_SECS),
            answer,
        )
        .await
        .map_err(|_| RegistryError::ApplicationIndeterminate(id.clone()))?
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(message)) => Err(RegistryError::Input {
                session_id: id.clone(),
                message,
            }),
            Err(_) => Err(RegistryError::Ended(id)),
        }
    }

    pub async fn resize(&self, cols: u16, rows: u16) -> Result<(), RegistryError> {
        let (reply, answer) = oneshot::channel();
        self.control(SessionCommand::Resize { cols, rows, reply }, answer)
            .await
    }

    pub async fn stop(&self) -> Result<(), RegistryError> {
        let (reply, answer) = oneshot::channel();
        self.control(SessionCommand::Stop { reply }, answer).await?;
        tokio::time::timeout(
            Duration::from_secs(gwk_domain::protocol::SLOW_CONSUMER_TIMEOUT_SECS),
            self.commands.closed(),
        )
        .await
        .map_err(|_| RegistryError::ApplicationIndeterminate(self.id.clone()))?;
        Ok(())
    }

    pub async fn apply_once<F, Fut>(
        &self,
        delivery_id: &EventId,
        operation: F,
    ) -> Result<(), RegistryError>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<(), RegistryError>>,
    {
        let mut delivered = self.delivered.lock().await;
        if delivered.contains(delivery_id) {
            return Ok(());
        }
        if delivered.order.len() >= DELIVERED_COMMANDS {
            return Err(RegistryError::DedupCapacity);
        }
        operation().await?;
        delivered.insert(delivery_id.clone());
        Ok(())
    }

    pub async fn applied_deliveries(&self) -> Vec<EventId> {
        self.delivered.lock().await.ids()
    }

    pub async fn settle_delivery(&self, delivery_id: &EventId) {
        self.delivered.lock().await.settle(delivery_id);
    }

    async fn control(
        &self,
        command: SessionCommand,
        answer: oneshot::Receiver<Result<(), String>>,
    ) -> Result<(), RegistryError> {
        tokio::time::timeout(
            Duration::from_secs(gwk_domain::protocol::SLOW_CONSUMER_TIMEOUT_SECS),
            self.commands.send(command),
        )
        .await
        .map_err(|_| RegistryError::ApplicationIndeterminate(self.id.clone()))?
        .map_err(|_| RegistryError::Ended(self.id.clone()))?;
        match tokio::time::timeout(
            Duration::from_secs(gwk_domain::protocol::SLOW_CONSUMER_TIMEOUT_SECS),
            answer,
        )
        .await
        .map_err(|_| RegistryError::ControlTimeout(self.id.clone()))?
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(message)) => Err(RegistryError::Input {
                session_id: self.id.clone(),
                message,
            }),
            Err(_) => Err(RegistryError::Ended(self.id.clone())),
        }
    }

    /// Exactly [`SessionRegistry::attach`], without the registry.
    pub async fn attach(&self, cursor: Option<u64>) -> Result<Attached, RegistryError> {
        let (reply, answer) = oneshot::channel();
        tokio::time::timeout(
            Duration::from_secs(gwk_domain::protocol::SLOW_CONSUMER_TIMEOUT_SECS),
            self.commands.send(SessionCommand::Attach { cursor, reply }),
        )
        .await
        .map_err(|_| RegistryError::ControlTimeout(self.id.clone()))?
        .map_err(|_| RegistryError::Ended(self.id.clone()))?;
        tokio::time::timeout(
            Duration::from_secs(gwk_domain::protocol::SLOW_CONSUMER_TIMEOUT_SECS),
            answer,
        )
        .await
        .map_err(|_| RegistryError::ControlTimeout(self.id.clone()))?
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
        for (ended_id, _) in self.reap() {
            tracing::debug!(session = %ended_id, "reaped ended session before spawn");
        }
        if self
            .sessions
            .get(&id)
            .is_some_and(|entry| entry.commands.is_closed())
        {
            // A routed stop waits for the session receiver to close only after
            // the child has been killed and reaped. The thread may still be in
            // its final runtime teardown, but it owns no live session state;
            // detach that spent handle so this id can start its next lifetime
            // without waiting for the resident reaper tick.
            self.sessions.remove(&id);
        }
        if self.sessions.contains_key(&id) {
            return Err(RegistryError::AlreadyExists(id));
        }
        if self.sessions.len() >= gwk_domain::protocol::PTY_SESSION_MAX_COUNT {
            return Err(RegistryError::Overloaded(
                gwk_domain::protocol::PTY_SESSION_MAX_COUNT,
            ));
        }
        let (commands, receiver) = mpsc::channel(DELTA_CHANNEL_CAPACITY);
        let (deltas_tx, _) = broadcast::channel(DELTA_CHANNEL_CAPACITY);
        let (raw_tx, _) = broadcast::channel(DELTA_CHANNEL_CAPACITY);
        let (ready_tx, ready_rx) = oneshot::channel::<Result<(), SpawnError>>();
        let delivered = std::sync::Arc::new(tokio::sync::Mutex::new(DeliveredCommands::default()));

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
                    run(session, spawn, config, receiver, deltas_tx, raw_tx).await
                })
            })
            .map_err(|e| RegistryError::Thread(e.to_string()))?;

        match tokio::time::timeout(
            Duration::from_secs(gwk_domain::protocol::SLOW_CONSUMER_TIMEOUT_SECS),
            ready_rx,
        )
        .await
        .map_err(|_| RegistryError::ControlTimeout(id.clone()))?
        {
            Ok(Ok(())) => {
                self.sessions.insert(
                    id,
                    Entry {
                        commands,
                        task,
                        delivered,
                    },
                );
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
        self.attacher(id)?.input(bytes).await
    }

    /// Resize a session's grid and PTY.
    pub async fn resize(
        &self,
        id: &PtySessionId,
        cols: u16,
        rows: u16,
    ) -> Result<(), RegistryError> {
        self.attacher(id)?.resize(cols, rows).await
    }

    /// The full screen at its current revision.
    pub async fn snapshot(&self, id: &PtySessionId) -> Result<Snapshot, RegistryError> {
        let (reply, answer) = oneshot::channel();
        self.send(id, SessionCommand::Snapshot { reply }).await?;
        tokio::time::timeout(
            Duration::from_secs(gwk_domain::protocol::SLOW_CONSUMER_TIMEOUT_SECS),
            answer,
        )
        .await
        .map_err(|_| RegistryError::ApplicationIndeterminate(id.clone()))?
        .map_err(|_| RegistryError::Ended(id.clone()))
    }

    /// Attach to a session's live output: `None` for a fresh consumer,
    /// `Some(seq)` to resume after that revision. Detaching is dropping the
    /// returned subscription; reattaching is calling this again with the
    /// last seq the consumer actually received.
    ///
    /// Two boundary facts, recorded where a later hand would reach to
    /// change them. A detach is a consumer-side event only — the child
    /// cannot observe one, because the session task's side of the PTY
    /// outlives every attachment (`tests/detach.rs` demonstrates each
    /// absent consequence). And there is no authorization here to extend:
    /// an attach carries a cursor and nothing else — no principal, no
    /// token, no per-consumer check. What admits a consumer at all is the
    /// kernel socket's peer-credential boundary (the kernel's listener
    /// refuses any peer that is not its own effective uid before a single
    /// byte is parsed — `gwk-kernel`'s `wire::listen`), and this host
    /// neither narrows nor widens that. A verb here that appeared to
    /// authorize would be claiming a boundary the host does not hold.
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
            delivered: std::sync::Arc::clone(&entry.delivered),
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
        // its real exit. Never hand a possibly permanent join to Tokio's
        // uncancellable blocking pool: wait on the nonblocking completion bit,
        // then join only after completion is certain.
        let (reply, _answer) = oneshot::channel();
        let _ = tokio::time::timeout(
            Duration::from_secs(gwk_domain::protocol::SLOW_CONSUMER_TIMEOUT_SECS),
            entry.commands.send(SessionCommand::Stop { reply }),
        )
        .await
        .map_err(|_| RegistryError::ControlTimeout(id.clone()))?;
        tokio::time::timeout(
            Duration::from_secs(gwk_domain::protocol::SLOW_CONSUMER_TIMEOUT_SECS),
            async {
                while !entry.task.is_finished() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            },
        )
        .await
        .map_err(|_| RegistryError::ApplicationIndeterminate(id.clone()))?;
        Ok(entry
            .task
            .join()
            .unwrap_or_else(|_| SessionExit::Failed("the session thread panicked".to_owned())))
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
        tokio::time::timeout(
            Duration::from_secs(gwk_domain::protocol::SLOW_CONSUMER_TIMEOUT_SECS),
            entry.commands.send(command),
        )
        .await
        .map_err(|_| RegistryError::ControlTimeout(id.clone()))?
        .map_err(|_| RegistryError::Ended(id.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{CatchUp, DeltaBatch, RawEvent, RestartPolicy};
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
                cells: frame.cells().expect("a served snapshot expands"),
            }
        }

        fn apply(&mut self, batch: &DeltaBatch) {
            for delta in batch.deltas.iter() {
                match delta {
                    PtyDelta::CellsChanged { styles, updates } => {
                        for update in updates {
                            self.cells[usize::from(update.row)][usize::from(update.col)] =
                                StyledCell {
                                    glyph: update.glyph.clone(),
                                    style: styles[update.style as usize].clone(),
                                };
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
        frame.cells().expect("a snapshot expands")[y]
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
        let mut raw_live = registry
            .attach(&id("s1"), None)
            .await
            .expect("raw observer")
            .raw_live;

        registry
            .input(&id("s1"), b"hello\n".to_vec())
            .await
            .expect("input");
        let snapshot = await_snapshot(&registry, &id("s1"), |s| {
            frame_row_text(&s.frame, 0).starts_with("hello")
        })
        .await;
        assert!(snapshot.seq.is_some(), "output must have produced frames");
        let raw = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let batch = raw_live.recv().await.expect("raw live stream");
                if let RawEvent::Output(bytes) = batch.event
                    && bytes.windows(5).any(|window| window == b"hello")
                {
                    return bytes;
                }
            }
        })
        .await
        .expect("the byte-exact output never arrived");
        assert!(raw.windows(5).any(|window| window == b"hello"));

        let raw_snapshot = registry
            .attach(&id("s1"), None)
            .await
            .expect("fresh raw snapshot")
            .raw_snapshot
            .expect("the grid formats a VT snapshot");
        assert!(
            raw_snapshot
                .bytes
                .windows(5)
                .any(|window| window == b"hello"),
            "the raw seed did not reproduce the current screen"
        );

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
        // Quiescence before the next write, not first sight: `cat` echoes the
        // line (row 0) AND writes its own copy (row 1). Sending "two" while
        // the copy is still in flight lets the echo of "two" land above it —
        // rows read one/two/one/two and the absolute positions asserted below
        // never hold.
        let cursor = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let batch = live.recv().await.expect("live stream");
                let seq = batch.seq;
                model.apply(&batch);
                if model.row_text(0).starts_with("one") && model.row_text(1).starts_with("one") {
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
            model.cells,
            current.frame.cells().expect("a snapshot expands"),
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
                (0..s.frame.rows.len()).any(|y| frame_row_text(&s.frame, y).starts_with(marker))
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
        let cells = snapshot.frame.cells().expect("a snapshot expands");
        assert_eq!(cells.len(), 30);
        assert!(cells.iter().all(|row| row.len() == 100));

        registry.stop(&id("s1")).await.expect("stop");
    }

    #[tokio::test]
    async fn the_resident_reaper_removes_an_attacher_stopped_session() {
        let registry = Arc::new(tokio::sync::Mutex::new(SessionRegistry::new()));
        registry
            .lock()
            .await
            .spawn(id("s1"), cat(), config())
            .await
            .expect("spawn");
        registry
            .lock()
            .await
            .attacher(&id("s1"))
            .expect("attacher")
            .stop()
            .await
            .expect("stop through the publisher handle");

        let publishers = Arc::new(tokio::sync::Mutex::new(tokio::task::JoinSet::new()));
        let (stop, stopped) = tokio::sync::watch::channel(false);
        let reaper = tokio::spawn(crate::control::serve_reaper(
            Arc::clone(&registry),
            Arc::clone(&publishers),
            stopped,
        ));

        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if !registry.lock().await.ids().contains(&id("s1")) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("stopped session is reaped");
        let _ = stop.send(true);
        reaper.await.expect("join reaper");
    }

    #[tokio::test]
    async fn a_stopped_session_id_can_restart_without_waiting_for_the_reaper_tick() {
        let mut registry = SessionRegistry::new();
        let session = id("restart-after-stop");
        registry
            .spawn(session.clone(), cat(), config())
            .await
            .expect("initial spawn");
        registry
            .attacher(&session)
            .expect("attacher")
            .stop()
            .await
            .expect("stop");

        registry
            .spawn(session.clone(), cat(), config())
            .await
            .expect("spawn reaps the ended predecessor first");
        registry.stop(&session).await.expect("stop replacement");
    }

    #[tokio::test]
    async fn the_registry_refuses_a_sixty_fifth_session_before_spawning_its_child() {
        let mut registry = SessionRegistry::new();
        let mut releases = Vec::new();
        for n in 0..gwk_domain::protocol::PTY_SESSION_MAX_COUNT {
            let (commands, _receiver) = mpsc::channel(1);
            let (release, wait) = std::sync::mpsc::channel();
            registry.sessions.insert(
                id(&format!("resident-{n}")),
                Entry {
                    commands,
                    task: std::thread::spawn(move || {
                        let _ = wait.recv();
                        SessionExit::Stopped
                    }),
                    delivered: Arc::new(tokio::sync::Mutex::new(DeliveredCommands::default())),
                },
            );
            releases.push(release);
        }
        let spawned = Arc::new(AtomicU32::new(0));
        let observed = Arc::clone(&spawned);
        let spawn: SpawnFn = Box::new(move |cols, rows| {
            observed.fetch_add(1, Ordering::SeqCst);
            gwk_pty::Session::spawn(pty_process::Command::new("/bin/cat"), cols, rows)
        });

        assert!(matches!(
            registry.spawn(id("overflow"), spawn, config()).await,
            Err(RegistryError::Overloaded(_))
        ));
        assert_eq!(
            spawned.load(Ordering::SeqCst),
            0,
            "no child may start past the cap"
        );
        for release in releases {
            let _ = release.send(());
        }
        let _ = registry.reap();
    }

    #[tokio::test]
    async fn one_command_id_is_applied_once_per_session_entry() {
        let mut registry = SessionRegistry::new();
        registry
            .spawn(id("dedup"), cat(), config())
            .await
            .expect("spawn");
        let first = registry.attacher(&id("dedup")).expect("first handle");
        let second = registry.attacher(&id("dedup")).expect("second handle");
        let applied = Arc::new(AtomicU32::new(0));

        for (handle, command) in [
            (&first, EventId::new("same")),
            (&second, EventId::new("same")),
            (&second, EventId::new("different")),
        ] {
            let applied = Arc::clone(&applied);
            handle
                .apply_once(&command, move || async move {
                    applied.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .await
                .expect("apply");
        }

        assert_eq!(applied.load(Ordering::SeqCst), 2);
        registry.stop(&id("dedup")).await.expect("stop");
    }

    #[tokio::test(start_paused = true)]
    async fn admitted_input_that_never_answers_is_indeterminate_at_the_deadline() {
        let (commands, mut receiver) = mpsc::channel(1);
        let attacher = Attacher {
            id: id("definitive-input"),
            commands,
            delivered: Arc::new(tokio::sync::Mutex::new(DeliveredCommands::default())),
        };
        let input = tokio::spawn(async move { attacher.input(b"x".to_vec()).await });
        let command = receiver.recv().await.expect("input is admitted");
        let SessionCommand::Input { reply, .. } = command else {
            panic!("unexpected command");
        };

        tokio::time::advance(Duration::from_secs(
            gwk_domain::protocol::SLOW_CONSUMER_TIMEOUT_SECS + 1,
        ))
        .await;
        tokio::task::yield_now().await;
        assert!(input.is_finished(), "the application wait must be bounded");
        assert!(matches!(
            input.await.expect("join input"),
            Err(RegistryError::ApplicationIndeterminate(_))
        ));
        assert!(
            reply.send(Ok(())).is_err(),
            "a late result cannot turn an indeterminate outcome into success"
        );
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
