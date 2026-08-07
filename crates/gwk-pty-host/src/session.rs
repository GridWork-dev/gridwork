//! One supervised PTY session: an engine child on a PTY, the recording of
//! its stream, the renderer that turns it into styled frames, and the wire
//! mirror consumers are served from — driven by one task that owns all four.
//!
//! The engine crate deliberately leaves the read loop to its caller
//! (`gwk_pty::Session`'s doc: "the read loop is the caller's"); this module
//! is that caller. One task per session, everything reached through its
//! command channel — the same `&mut self`-discipline the engine uses, with
//! the channel standing in for the borrow.
//!
//! Derivation: none — process supervision and channel plumbing over the
//! engine crate's own API; no terminal-protocol byte is parsed here (the
//! engine's parser owns that) and every PTY fact used is one the engine
//! already exposes as typed output.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gwk_domain::frame::{PtyDelta, PtyFrame};
use gwk_pty::record::{Event, Recording};
use gwk_pty::render::Renderer;
use gwk_pty::{Session, SpawnError};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::wire::WireScreen;

/// How a session's engine child is (re)started. A plain `fn` for the three
/// real engines (`crate::engines`), a closure for tests — either way the
/// factory outlives the first child, because restart calls it again.
pub type SpawnFn = Box<dyn Fn(u16, u16) -> Result<Session, SpawnError> + Send + Sync>;

/// Everything a session is configured with at spawn.
pub struct SessionConfig {
    pub cols: u16,
    pub rows: u16,
    /// Entries the session's [`Recording`] retains.
    pub recording_cap: usize,
    /// Delta batches retained for gapless reattach; a cursor older than the
    /// oldest retained batch falls back to a snapshot, mirroring
    /// `Recording::since`'s eviction refusal.
    pub retained_batches: usize,
    pub restart: RestartPolicy,
}

/// What happens when the engine child exits on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPolicy {
    /// The session ends with the child.
    Never,
    /// A FAILED child is respawned, up to `max` times, `delay` apart. A
    /// clean exit ends the session either way — restarting a child that
    /// meant to stop would fight it.
    ///
    /// ponytail: a fixed delay, not exponential backoff — the unit's own
    /// `RestartSec` takes over once the whole host is what's failing. Add
    /// backoff if a flapping engine ever measures as a real cost.
    OnFailure { max: u32, delay: Duration },
}

/// One broadcast batch: the deltas that move a consumer from before `seq`
/// to `seq`. Batches for sequences that changed nothing on the wire are
/// never sent, so gaps between consecutive batch seqs are no-ops by
/// construction — a resume needs every retained batch AFTER its cursor,
/// nothing else.
#[derive(Debug, Clone)]
pub struct DeltaBatch {
    pub seq: u64,
    pub deltas: Arc<Vec<PtyDelta>>,
}

/// The full screen at a revision. `seq` is absent only while the session
/// has produced no frame yet, matching `PtyAttached.cursor`'s contract.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub seq: Option<u64>,
    pub frame: PtyFrame,
}

/// How an attach was brought current, mirroring `gwk_pty::CaughtUp` at the
/// wire level.
#[derive(Debug)]
pub enum CatchUp {
    /// The retained batches covered the gap; apply them in order.
    Replayed(Vec<DeltaBatch>),
    /// The gap was evicted (or the attach was fresh): reseed from the frame.
    Snapshotted(Snapshot),
}

/// A live attachment: the catch-up that brings the consumer current and the
/// subscription that keeps it there. Dropping `live` IS detaching — there is
/// no unregister step, which is what makes detach/reattach routing a cursor
/// question rather than a bookkeeping one.
#[derive(Debug)]
pub struct Attached {
    pub cols: u16,
    pub rows: u16,
    /// The revision the consumer is current to after the catch-up — what a
    /// reattach after a disconnect resumes from.
    pub cursor: Option<u64>,
    pub catch_up: CatchUp,
    pub live: broadcast::Receiver<DeltaBatch>,
}

/// Why a session's task ended.
#[derive(Debug)]
pub enum SessionExit {
    /// The child exited (cleanly, or with restarts exhausted).
    Exited(std::process::ExitStatus),
    /// [`SessionCommand::Stop`] — the child was killed and reaped.
    Stopped,
    /// The PTY read failed, a respawn failed, or the renderer gave out.
    Failed(String),
}

/// Everything a session can be asked to do while it runs.
pub enum SessionCommand {
    /// Bytes for the child, as a terminal would send them.
    Input(Vec<u8>),
    Resize {
        cols: u16,
        rows: u16,
    },
    Snapshot {
        reply: oneshot::Sender<Snapshot>,
    },
    Attach {
        /// `None` is a fresh attach; `Some` resumes after that revision.
        cursor: Option<u64>,
        reply: oneshot::Sender<Attached>,
    },
    Stop,
}

/// Capacity of the live-delta broadcast. A consumer that falls this many
/// batches behind sees `Lagged` and reattaches from its cursor — the same
/// recovery the wire contract already demands of a slow consumer.
pub(crate) const DELTA_CHANNEL_CAPACITY: usize = 64;

/// The session task. Owns the child, the recording, the renderer, and the
/// mirror; runs until the child ends (per policy), a `Stop` arrives, or the
/// command channel closes (every handle dropped).
pub(crate) async fn run(
    mut session: Session,
    spawn: SpawnFn,
    config: SessionConfig,
    mut commands: mpsc::Receiver<SessionCommand>,
    deltas_tx: broadcast::Sender<DeltaBatch>,
) -> SessionExit {
    let started = Instant::now();
    let mut recording = Recording::new(config.recording_cap);
    let Some(mut renderer) = Renderer::new() else {
        return SessionExit::Failed("could not allocate the render state".to_owned());
    };
    let mut screen = WireScreen::new(config.cols, config.rows);
    let mut retained: VecDeque<DeltaBatch> = VecDeque::new();
    // The highest seq ever evicted from `retained` — the eviction horizon a
    // reattach cursor is checked against, mirroring `Recording::since`.
    let mut evicted_through: Option<u64> = None;
    let mut last_seq: Option<u64> = None;
    let mut restarts_left = match config.restart {
        RestartPolicy::Never => 0,
        RestartPolicy::OnFailure { max, .. } => max,
    };

    loop {
        // The select produces a value and DROPS its losing future before the
        // handling below touches `session` again — which is what lets one
        // `&mut` session serve both arms without a lock.
        enum Wake {
            Chunk(std::io::Result<Vec<u8>>),
            Command(Option<SessionCommand>),
        }
        let wake = tokio::select! {
            chunk = session.pump_chunk() => Wake::Chunk(chunk),
            command = commands.recv() => Wake::Command(command),
        };

        match wake {
            Wake::Chunk(Ok(chunk)) if chunk.is_empty() => {
                let status = match session.wait().await {
                    Ok(status) => status,
                    Err(e) => return SessionExit::Failed(format!("reaping the child: {e}")),
                };
                let failed = !status.success();
                if !(failed && restarts_left > 0) {
                    tracing::info!(%status, "engine child exited; session ends");
                    return SessionExit::Exited(status);
                }
                restarts_left -= 1;
                let RestartPolicy::OnFailure { delay, .. } = config.restart else {
                    return SessionExit::Exited(status);
                };
                tracing::warn!(%status, restarts_left, "engine child failed; restarting");
                tokio::time::sleep(delay).await;
                let (cols, rows) = screen.size();
                session = match spawn(cols, rows) {
                    Ok(session) => session,
                    Err(e) => return SessionExit::Failed(format!("respawn: {e}")),
                };
                renderer = match Renderer::new() {
                    Some(renderer) => renderer,
                    None => {
                        return SessionExit::Failed(
                            "could not allocate a render state for the restarted child".to_owned(),
                        );
                    }
                };
                // The new child starts on a blank screen. Consumers are told
                // exactly the way the contract already handles invalidated
                // state: a `Resized` (to the same size) means every held
                // coordinate is void and a snapshot re-seed is due. Recorded
                // as a resize too — the respawn really did size a fresh PTY,
                // and that fact travels outside the byte stream.
                let seq = recording.record(offset_ms(started), Event::Resize { cols, rows });
                screen = WireScreen::new(cols, rows);
                last_seq = Some(seq);
                publish(
                    DeltaBatch {
                        seq,
                        deltas: Arc::new(vec![PtyDelta::Resized { rows, cols }]),
                    },
                    &deltas_tx,
                    &mut retained,
                    &mut evicted_through,
                    config.retained_batches,
                );
            }
            Wake::Chunk(Ok(chunk)) => {
                let seq = recording.record(offset_ms(started), Event::Output(chunk));
                last_seq = Some(seq);
                let Some(frame) = renderer.frame(session.grid(), seq) else {
                    return SessionExit::Failed("the renderer could not read the grid".to_owned());
                };
                let deltas = screen.apply(&frame);
                if has_content(&deltas) {
                    publish(
                        DeltaBatch {
                            seq,
                            deltas: Arc::new(deltas),
                        },
                        &deltas_tx,
                        &mut retained,
                        &mut evicted_through,
                        config.retained_batches,
                    );
                }
            }
            Wake::Chunk(Err(e)) => {
                return SessionExit::Failed(format!("reading the pty: {e}"));
            }
            Wake::Command(None) | Wake::Command(Some(SessionCommand::Stop)) => {
                if let Err(e) = session.kill().await {
                    tracing::warn!(%e, "killing the child on stop");
                }
                let _ = session.wait().await;
                return SessionExit::Stopped;
            }
            Wake::Command(Some(SessionCommand::Input(bytes))) => {
                if let Err(e) = session.send(&bytes).await {
                    tracing::warn!(%e, "input did not reach the child");
                }
            }
            Wake::Command(Some(SessionCommand::Resize { cols, rows })) => {
                match session.resize(cols, rows) {
                    Ok(()) => {
                        // The reflow marked the grid fully dirty; take the
                        // frame NOW rather than waiting for the child to
                        // write — a resize with a quiet child must still
                        // reach consumers.
                        let seq =
                            recording.record(offset_ms(started), Event::Resize { cols, rows });
                        last_seq = Some(seq);
                        let Some(frame) = renderer.frame(session.grid(), seq) else {
                            return SessionExit::Failed(
                                "the renderer could not read the resized grid".to_owned(),
                            );
                        };
                        let deltas = screen.apply(&frame);
                        publish(
                            DeltaBatch {
                                seq,
                                deltas: Arc::new(deltas),
                            },
                            &deltas_tx,
                            &mut retained,
                            &mut evicted_through,
                            config.retained_batches,
                        );
                    }
                    Err(e) => tracing::warn!(%e, cols, rows, "resize refused"),
                }
            }
            Wake::Command(Some(SessionCommand::Snapshot { reply })) => {
                let _ = reply.send(Snapshot {
                    seq: last_seq,
                    frame: screen.snapshot(),
                });
            }
            Wake::Command(Some(SessionCommand::Attach { cursor, reply })) => {
                // Subscribe before building the catch-up. Inside this task
                // nothing races — the order just makes the guarantee local:
                // every batch after the catch-up point is either in the
                // catch-up or in the subscription, never in neither.
                let live = deltas_tx.subscribe();
                let catch_up = catch_up(cursor, last_seq, evicted_through, &retained, &screen);
                let (cols, rows) = screen.size();
                let _ = reply.send(Attached {
                    cols,
                    rows,
                    cursor: last_seq,
                    catch_up,
                    live,
                });
            }
        }
    }
}

/// Decide how an attach at `cursor` is brought current — the wire-level
/// [`gwk_pty::attach::catch_up`]: replay when the retained stream reaches
/// back far enough, snapshot when it does not (or the attach is fresh).
fn catch_up(
    cursor: Option<u64>,
    last_seq: Option<u64>,
    evicted_through: Option<u64>,
    retained: &VecDeque<DeltaBatch>,
    screen: &WireScreen,
) -> CatchUp {
    let snapshot = || {
        CatchUp::Snapshotted(Snapshot {
            seq: last_seq,
            frame: screen.snapshot(),
        })
    };
    let Some(cursor) = cursor else {
        return snapshot();
    };
    // A cursor past the live head names a stream position this session never
    // produced — a consumer holding state from some earlier life. Reset it.
    if last_seq.is_none_or(|last| cursor > last) {
        return snapshot();
    }
    // A batch newer than the cursor was evicted: the gap cannot be served.
    if evicted_through.is_some_and(|evicted| cursor < evicted) {
        return snapshot();
    }
    CatchUp::Replayed(
        retained
            .iter()
            .filter(|batch| batch.seq > cursor)
            .cloned()
            .collect(),
    )
}

/// Broadcast one batch and retain it for reattach, evicting past the cap.
fn publish(
    batch: DeltaBatch,
    deltas_tx: &broadcast::Sender<DeltaBatch>,
    retained: &mut VecDeque<DeltaBatch>,
    evicted_through: &mut Option<u64>,
    cap: usize,
) {
    retained.push_back(batch.clone());
    while retained.len() > cap {
        if let Some(dropped) = retained.pop_front() {
            *evicted_through = Some(dropped.seq);
        }
    }
    // An error only means no consumer is attached right now, which is an
    // ordinary state for a resident host, not a failure.
    let _ = deltas_tx.send(batch);
}

/// Does a batch move a consumer at all?
fn has_content(deltas: &[PtyDelta]) -> bool {
    deltas.iter().any(|delta| match delta {
        PtyDelta::CellsChanged { updates } => !updates.is_empty(),
        PtyDelta::Resized { .. } => true,
    })
}

fn offset_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}
