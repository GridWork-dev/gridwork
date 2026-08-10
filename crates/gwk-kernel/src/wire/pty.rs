//! The kernel's PTY session registry — the hub between one publishing host
//! and every attached consumer.
//!
//! The PTY surface is the one part of this wire that is not backed by the
//! log: a session's frames are ephemeral by design (the recording that
//! persists is the host's concern, and a different feature). So where every
//! other request is answered from PostgreSQL, the PTY family is answered
//! from here — an in-memory map the publishing host keeps current and
//! consumers read, with the same catch-up-or-reseed semantics the contract's
//! `PtyAttach` doc describes.
//!
//! # Ownership is the connection
//!
//! The first `pty_publish_snapshot` for a session claims it for the
//! connection that sent it; publishes from any other live connection are
//! refused, and every session a connection claimed retires when that
//! connection closes. That is the whole authorization story, and it is
//! deliberate: the UDS peer credential already gates who may connect at all
//! (ADR 0002), so what is left to enforce here is not identity but
//! single-writer coherence — two publishers interleaving revisions of one
//! screen would corrupt it for every consumer. Retire-on-hangup is also what
//! makes a crashed host recoverable without an operator: its replacement
//! connects and claims the same ids fresh.
//!
//! # What a consumer is promised
//!
//! An attach inside the retained window replays exactly the batches after
//! its cursor; an attach whose gap was evicted (or whose cursor names a
//! revision this session's life never produced) is answered at the live head
//! with no replay, which the client reads as "reseed from a snapshot" — the
//! same fallback `gwk-pty`'s own attach module takes on eviction. A slow
//! consumer loses its stream, not its connection, closed with the last
//! revision that actually reached the socket.
//!
//! No terminal byte is interpreted anywhere in this module: frames and
//! deltas are typed wire values produced and consumed elsewhere; the hub
//! only stores, validates bounds, and fans out.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use gwk_domain::frame::{CellStyle, PtyDelta, PtyFrame, PtyRun, StyledCell};
use gwk_domain::ids::{PtyFrameSeq, PtySessionGeneration, PtySessionId, RequestId, WriterEpoch};
use gwk_domain::protocol::{
    FRAME_BODY_MAX_BYTES, KernelErrorCode, KernelResult, SLOW_CONSUMER_TIMEOUT_SECS, ServerControl,
};
use tokio::sync::{broadcast, mpsc};

use super::serve::Outgoing;

/// Delta batches one session retains for gapless reattach. A cursor older
/// than the oldest retained batch falls back to a snapshot reseed, exactly
/// as the host's own registry does when its window is exhausted.
const RETAINED_BATCHES: usize = 256;

/// Serialized bytes one session's retained window may hold. The count above
/// bounds the common case (keystroke-sized batches); this bounds the
/// pathological one (full-screen repaints), so a session's memory ceiling is
/// a number rather than a hope. Eviction from the front on either bound.
const RETAINED_BYTES: usize = 8 * 1024 * 1024;

/// Serialized bytes one publish may carry. Half the frame cap, for the same
/// reason `PAGE_BYTE_BUDGET` is half: what came in as a request body goes
/// back out wrapped in a response or batch envelope, and a value admitted at
/// the ingress cap could then never be re-served. Enforced against the
/// serialized frame or delta list, measured once at publish.
const PUBLISH_BYTE_BUDGET: usize = (FRAME_BODY_MAX_BYTES as usize) / 2;

/// Serialized bytes one cell update may carry. A glyph is one grapheme
/// cluster — tens of bytes at its ZWJ-emoji worst — so this bound never
/// touches a real screen; what it bounds is the mirror's growth, which
/// accumulates across batches and is measured by no single publish. The
/// area's own bound is the u16 grid universe the strict decode enforces —
/// under the interned shape a publish's bytes no longer scale with its
/// area, so the byte budget stopped being an area bound; the publisher
/// behind the same-EUID socket is the resident host, and the geometry it
/// declares is the operator's own.
const MAX_GLYPH_BYTES: usize = 64;

/// Capacity of a session's live broadcast. A consumer that falls this many
/// batches behind is closed as a slow consumer and reattaches from its
/// cursor — the recovery the wire contract already demands of one.
const BROADCAST_CAPACITY: usize = 64;

/// Most sessions the hub holds at once, across every publisher. Each session
/// carries a mirror and up to [`RETAINED_BYTES`] of window, so without a
/// count the kernel's PTY memory is a function of a publisher's behaviour —
/// the same reason `MAX_SUBSCRIPTIONS_PER_CONNECTION` exists. Exceeding it
/// refuses the claiming publish with `overloaded` and touches nothing else;
/// a box hosting anywhere near this many live engine TUIs has other
/// problems first.
const MAX_SESSIONS: usize = 64;

/// One wire connection's identity inside the hub, minted at accept time.
/// Never reused within a process lifetime, which is what lets "the owner is
/// gone" and "the owner is someone else" be the same comparison.
pub type ConnectionId = u64;

/// One broadcast batch: the deltas that move a consumer to revision `seq`.
#[derive(Debug, Clone)]
pub struct PtyBatch {
    pub seq: u64,
    pub deltas: Arc<Vec<PtyDelta>>,
}

/// A refusal from a hub verb, in the contract's own error vocabulary.
#[derive(Debug)]
pub struct PtyRefusal {
    pub code: KernelErrorCode,
    pub message: String,
    pub detail: Option<serde_json::Value>,
}

impl PtyRefusal {
    fn new(code: KernelErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            detail: None,
        }
    }

    /// The value form every serve arm answers with.
    pub fn into_result(self) -> KernelResult {
        KernelResult::Error {
            code: self.code,
            message: self.message,
            detail: self.detail,
        }
    }
}

/// What an accepted attach hands the serve layer: the answer's fields, the
/// catch-up to send first, and the live subscription that follows it.
#[derive(Debug)]
pub struct Attached {
    /// The session id's current life. A consumer must pair this with `cursor`.
    pub generation: PtySessionGeneration,
    pub rows: u16,
    pub cols: u16,
    /// The revision deltas resume from — the `PtyAttached` answer. `None`
    /// only for a session that has produced no frame yet.
    pub cursor: Option<u64>,
    /// Retained batches covering (cursor, head], oldest first. Empty for a
    /// fresh attach, and for a cursor the window could not serve — the
    /// `cursor` field is how the client tells those apart.
    pub replay: Vec<PtyBatch>,
    pub live: broadcast::Receiver<PtyBatch>,
}

/// One hosted session's state, exactly what the host published.
struct Session {
    owner: ConnectionId,
    generation: PtySessionGeneration,
    /// Head revision; `None` until the first sequenced publish.
    seq: Option<u64>,
    /// The mirror snapshots are answered from. Rectangular by construction:
    /// a publish that would break that is refused before anything changes.
    cells: Vec<Vec<StyledCell>>,
    retained: VecDeque<(PtyBatch, usize)>,
    retained_bytes: usize,
    /// The highest seq ever evicted from `retained` — the horizon a reattach
    /// cursor is checked against.
    evicted_through: Option<u64>,
    live: broadcast::Sender<PtyBatch>,
}

impl Session {
    fn size(&self) -> (u16, u16) {
        // The publish path refused any grid that does not fit u16 before it
        // reached the mirror, so these casts cannot truncate.
        let rows = self.cells.len() as u16;
        let cols = self.cells.first().map_or(0, |row| row.len() as u16);
        (cols, rows)
    }

    fn retain(&mut self, batch: PtyBatch, bytes: usize) {
        self.retained_bytes += bytes;
        self.retained.push_back((batch, bytes));
        while self.retained.len() > RETAINED_BATCHES || self.retained_bytes > RETAINED_BYTES {
            let Some((dropped, dropped_bytes)) = self.retained.pop_front() else {
                break;
            };
            self.retained_bytes -= dropped_bytes;
            self.evicted_through = Some(dropped.seq);
        }
    }

    fn publish(&mut self, batch: PtyBatch, bytes: usize) {
        self.retain(batch.clone(), bytes);
        // An error only means nobody is attached right now — an ordinary
        // state for a resident surface, not a failure.
        let _ = self.live.send(batch);
    }
}

/// Every hosted session, by id. One per daemon, shared by every connection.
pub struct PtyHub {
    sessions: Mutex<HashMap<PtySessionId, Session>>,
    connections: AtomicU64,
    writer_epoch: WriterEpoch,
    generations: AtomicU64,
}

#[cfg(test)]
impl Default for PtyHub {
    fn default() -> Self {
        Self::new(WriterEpoch::new(0))
    }
}

impl PtyHub {
    /// Start an empty registry in this daemon's durable writer epoch.
    /// Generation tokens combine that epoch with a process-local counter, so
    /// neither a reclaimed id nor a restarted daemon can reuse a session life.
    pub fn new(writer_epoch: WriterEpoch) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            connections: AtomicU64::new(0),
            writer_epoch,
            generations: AtomicU64::new(0),
        }
    }

    /// Mint one connection's identity. Starts at 1 so no live connection can
    /// ever equal a zero-initialized field by accident.
    pub fn connection(&self) -> ConnectionId {
        self.connections.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Publish a full screen: claim (first publish), reseed (owner, at or
    /// past the head), and the dimensions every later bound is checked
    /// against.
    pub fn publish_snapshot(
        &self,
        conn: ConnectionId,
        id: &PtySessionId,
        seq: Option<u64>,
        frame: PtyFrame,
    ) -> Result<(), PtyRefusal> {
        let (rows, cols, cells) = strict_frame(&frame)?;
        let bytes = measured(&frame)?;
        let mut sessions = self.lock();
        match sessions.get_mut(id) {
            None => {
                if sessions.len() >= MAX_SESSIONS {
                    return Err(PtyRefusal::new(
                        KernelErrorCode::Overloaded,
                        format!("the hub already holds {MAX_SESSIONS} pty sessions"),
                    ));
                }
                let generation = self.next_generation()?;
                let (live, _) = broadcast::channel(BROADCAST_CAPACITY);
                sessions.insert(
                    id.clone(),
                    Session {
                        owner: conn,
                        generation,
                        seq,
                        cells,
                        retained: VecDeque::new(),
                        retained_bytes: 0,
                        // The claim's whole history is evicted by definition:
                        // this session may be a RECLAIM of an id whose earlier
                        // life a consumer still holds a cursor into, and the
                        // batches between that cursor and this head are gone
                        // with the old entry. Seeding the horizon at the head
                        // is what keeps the attach answer honest — a cursor
                        // below it reseeds instead of being "honored" over a
                        // silent gap.
                        evicted_through: seq,
                        live,
                    },
                );
                Ok(())
            }
            Some(session) => {
                owned(session, conn, id)?;
                match (session.seq, seq) {
                    // Revisions never move backwards, and a sequenced screen
                    // cannot return to "no frame yet".
                    (Some(head), Some(seq)) if seq < head => {
                        return Err(backwards(id, seq, head));
                    }
                    (Some(head), None) => {
                        // Stale-view class, like `backwards`: the publisher
                        // thinks the session is pre-first-frame and it is not.
                        return Err(PtyRefusal::new(
                            KernelErrorCode::StaleVersion,
                            format!(
                                "pty session {id} is at revision {head}; \
                                 an unsequenced snapshot cannot replace it"
                            ),
                        ));
                    }
                    _ => {}
                }
                let advanced = match (session.seq, seq) {
                    (Some(head), Some(seq)) => seq > head,
                    (None, Some(_)) => true,
                    _ => false,
                };
                if !advanced {
                    // A reseed at the head re-announces the screen a revision
                    // already names; two dimensions under one revision would
                    // be two screens, and the consumers who hold the first
                    // would never hear about the second (a resize broadcast
                    // rides a seq, and this publish does not advance one).
                    let (old_cols, old_rows) = session.size();
                    if (rows, cols) != (old_rows, old_cols) {
                        return Err(PtyRefusal::new(
                            KernelErrorCode::Validation,
                            format!(
                                "pty session {id}: a reseed that does not advance the \
                                 revision must keep its {old_rows}x{old_cols} dimensions"
                            ),
                        ));
                    }
                }
                session.cells = cells;
                session.seq = seq;
                if advanced {
                    // The head moved without deltas covering the distance.
                    // Consumers are told the way the contract already spells
                    // "every coordinate you hold is void": a resize (to the
                    // new size, which may equal the old), after which a
                    // client re-requests a snapshot.
                    let batch = PtyBatch {
                        seq: seq.expect("advanced implies a sequenced publish"),
                        deltas: Arc::new(vec![PtyDelta::Resized { rows, cols }]),
                    };
                    session.publish(batch, bytes);
                }
                Ok(())
            }
        }
    }

    /// Publish the batch that moves a session to `seq`: owner-only, strictly
    /// forward, every update inside the grid it lands on. Refused whole on
    /// any violation — a batch half-applied would desynchronize the mirror
    /// from every consumer's copy.
    pub fn publish_deltas(
        &self,
        conn: ConnectionId,
        id: &PtySessionId,
        seq: u64,
        deltas: Vec<PtyDelta>,
    ) -> Result<(), PtyRefusal> {
        let bytes = measured(&deltas)?;
        let mut sessions = self.lock();
        let session = sessions.get_mut(id).ok_or_else(|| unknown_session(id))?;
        owned(session, conn, id)?;
        if let Some(head) = session.seq
            && seq <= head
        {
            return Err(backwards(id, seq, head));
        }

        // Validate the whole batch against the dimensions it would see,
        // before any of it is applied.
        let (mut cols, mut rows) = session.size();
        for delta in &deltas {
            match delta {
                PtyDelta::Resized {
                    rows: new_rows,
                    cols: new_cols,
                } => {
                    // The delta itself is bytes-tiny, but applying it
                    // allocates a full blank grid — so the grid it names is
                    // held to the same budget a snapshot publish of that grid
                    // would face, BEFORE anything is allocated. Without this,
                    // a 40-byte frame could demand a u16::MAX-squared mirror.
                    let grid = blank_grid_bytes(*new_rows, *new_cols);
                    if grid > PUBLISH_BYTE_BUDGET as u64 {
                        return Err(PtyRefusal::new(
                            KernelErrorCode::Validation,
                            format!(
                                "pty session {id}: a resize to {new_rows}x{new_cols} names \
                                 a {grid}-byte screen, past the {PUBLISH_BYTE_BUDGET}-byte \
                                 budget a snapshot of it would be held to"
                            ),
                        ));
                    }
                    rows = *new_rows;
                    cols = *new_cols;
                }
                PtyDelta::CellsChanged { styles, updates } => {
                    duplicate_free(styles)?;
                    for update in updates {
                        if usize::try_from(update.style)
                            .ok()
                            .is_none_or(|index| index >= styles.len())
                        {
                            return Err(out_of_range(update.style, styles.len()));
                        }
                        if update.row >= rows || update.col >= cols {
                            return Err(PtyRefusal::new(
                                KernelErrorCode::Validation,
                                format!(
                                    "pty session {id}: update at ({}, {}) is outside \
                                     the {rows}x{cols} grid",
                                    update.row, update.col
                                ),
                            ));
                        }
                        if update.glyph.len() > MAX_GLYPH_BYTES {
                            return Err(PtyRefusal::new(
                                KernelErrorCode::Validation,
                                format!(
                                    "pty session {id}: a {}-byte glyph at ({}, {}) exceeds \
                                     the {MAX_GLYPH_BYTES}-byte cell bound",
                                    update.glyph.len(),
                                    update.row,
                                    update.col
                                ),
                            ));
                        }
                    }
                }
            }
        }

        for delta in &deltas {
            match delta {
                PtyDelta::Resized { rows, cols } => {
                    session.cells = vec![vec![blank(); usize::from(*cols)]; usize::from(*rows)];
                }
                PtyDelta::CellsChanged { styles, updates } => {
                    // Resolve each update through the batch's own table and
                    // forget the table: the mirror stays a plain grid, and no
                    // index space outlives the message that carried it.
                    for update in updates {
                        session.cells[usize::from(update.row)][usize::from(update.col)] =
                            StyledCell {
                                glyph: update.glyph.clone(),
                                style: styles[update.style as usize].clone(),
                            };
                    }
                }
            }
        }
        session.seq = Some(seq);
        session.publish(
            PtyBatch {
                seq,
                deltas: Arc::new(deltas),
            },
            bytes,
        );
        Ok(())
    }

    /// Retire one session the connection claimed. Dropping the entry drops
    /// its broadcast sender, which is what closes every attached stream.
    pub fn retire(&self, conn: ConnectionId, id: &PtySessionId) -> Result<(), PtyRefusal> {
        let mut sessions = self.lock();
        let session = sessions.get(id).ok_or_else(|| unknown_session(id))?;
        owned(session, conn, id)?;
        sessions.remove(id);
        Ok(())
    }

    /// Retire everything the connection claimed — the hangup path.
    pub fn disconnect(&self, conn: ConnectionId) {
        self.lock().retain(|_, session| session.owner != conn);
    }

    /// Attach a consumer. The subscription and the replay are taken under
    /// one lock, so every batch after the catch-up point is in exactly one
    /// of them.
    pub fn attach(
        &self,
        id: &PtySessionId,
        generation: Option<&PtySessionGeneration>,
        cursor: Option<u64>,
    ) -> Result<Attached, PtyRefusal> {
        let sessions = self.lock();
        let session = sessions.get(id).ok_or_else(|| unknown_session(id))?;
        let (cols, rows) = session.size();
        let live = session.live.subscribe();

        let serviceable = match (generation, cursor) {
            (Some(generation), Some(cursor)) if generation == &session.generation => {
                // A revision this session's life never produced, or one whose
                // gap was evicted: answered at the live head with no replay,
                // which the client reads as "your cursor was not honored —
                // reseed". The same two fallbacks the host's own catch-up takes.
                let past_head = session.seq.is_none_or(|head| cursor > head);
                let evicted = session
                    .evicted_through
                    .is_some_and(|evicted| cursor < evicted);
                (!past_head && !evicted).then_some(cursor)
            }
            _ => None,
        };
        let (cursor_answer, replay) = match serviceable {
            Some(cursor) => (
                Some(cursor),
                session
                    .retained
                    .iter()
                    .filter(|(batch, _)| batch.seq > cursor)
                    .map(|(batch, _)| batch.clone())
                    .collect(),
            ),
            None => (session.seq, Vec::new()),
        };
        Ok(Attached {
            generation: session.generation.clone(),
            rows,
            cols,
            cursor: cursor_answer,
            replay,
            live,
        })
    }

    /// The full screen at its current revision, or a typed refusal: an
    /// unknown id, a session that has produced no frame to serve yet, or —
    /// the guard on the way OUT — a mirror whose serialized form outgrew
    /// what one response frame carries. The ingress budget bounds every
    /// single publish, but cell updates accumulate into the mirror across
    /// batches without any one of them being the oversized one; measuring
    /// here turns what would be a fatal framing error at the write (tearing
    /// down the whole connection over a well-formed request) into a value
    /// the client can branch on.
    pub fn snapshot(
        &self,
        id: &PtySessionId,
    ) -> Result<(PtySessionGeneration, u64, PtyFrame), PtyRefusal> {
        let sessions = self.lock();
        let session = sessions.get(id).ok_or_else(|| unknown_session(id))?;
        let seq = session.seq.ok_or_else(|| {
            PtyRefusal::new(
                KernelErrorCode::NotFound,
                format!("pty session {id} has produced no frame yet"),
            )
        })?;
        // A fresh first-appearance-order table per answer: the mirror holds
        // no persistent index space, so serving is where interning happens.
        let frame = PtyFrame::from_cells(&session.cells);
        let bytes = serde_json::to_vec(&frame)
            .map_err(|e| {
                PtyRefusal::new(KernelErrorCode::Storage, format!("measure a snapshot: {e}"))
            })?
            .len();
        if bytes > PUBLISH_BYTE_BUDGET {
            return Err(PtyRefusal::new(
                KernelErrorCode::Overloaded,
                format!(
                    "pty session {id}: the screen serializes to {bytes} bytes, past the \
                     {PUBLISH_BYTE_BUDGET}-byte bound one response frame carries; attach \
                     for deltas instead"
                ),
            ));
        }
        Ok((session.generation.clone(), seq, frame))
    }

    fn next_generation(&self) -> Result<PtySessionGeneration, PtyRefusal> {
        let counter = self
            .generations
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| {
                PtyRefusal::new(
                    KernelErrorCode::Overloaded,
                    "the PTY session generation space is exhausted",
                )
            })?
            + 1;
        Ok(PtySessionGeneration::new(format!(
            "{}:{counter}",
            self.writer_epoch
        )))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PtySessionId, Session>> {
        // A poisoned map means a panic mid-verb on another connection. The
        // map's invariants hold between verbs (each verb validates before it
        // mutates), so serving beats taking the whole PTY surface down.
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Deliver one attach: replay first, then the live stream, until the
/// consumer hangs up, falls behind, or the session retires.
///
/// The close travels on `responses` — the priority queue — for the same
/// reason a subscription's `StreamClosed` does: the frame that explains why
/// nothing else is coming must get past a batch queue the stopped consumer
/// left full. `delivered` holds `seq + 1` (0 = nothing sent), written by the
/// write loop after each frame actually leaves, so the close reports what
/// the client has SEEN rather than what was queued.
pub(crate) async fn run_attach(
    request_id: RequestId,
    session_id: PtySessionId,
    attached: Attached,
    batches: mpsc::Sender<Outgoing>,
    responses: mpsc::Sender<ServerControl>,
    delivered: Arc<AtomicU64>,
) {
    let Attached {
        generation,
        replay,
        mut live,
        ..
    } = attached;

    enum Ended {
        SlowConsumer,
        /// The session retired: its broadcast sender was dropped.
        Retired,
        /// The connection went away; nobody is left to tell.
        Gone,
    }

    let mut ended = None;
    for batch in replay {
        match send(
            &batches,
            &request_id,
            &session_id,
            &generation,
            batch,
            &delivered,
        )
        .await
        {
            Ok(()) => {}
            Err(gone) => {
                ended = Some(if gone {
                    Ended::Gone
                } else {
                    Ended::SlowConsumer
                });
                break;
            }
        }
    }
    while ended.is_none() {
        match live.recv().await {
            Ok(batch) => match send(
                &batches,
                &request_id,
                &session_id,
                &generation,
                batch,
                &delivered,
            )
            .await
            {
                Ok(()) => {}
                Err(gone) => {
                    ended = Some(if gone {
                        Ended::Gone
                    } else {
                        Ended::SlowConsumer
                    });
                }
            },
            // Falling behind the broadcast loses batches, and a stream that
            // silently skipped revisions would corrupt the consumer's mirror
            // — so it ends, typed, and the client reattaches from its cursor.
            Err(broadcast::error::RecvError::Lagged(_)) => {
                ended = Some(Ended::SlowConsumer);
            }
            Err(broadcast::error::RecvError::Closed) => {
                ended = Some(Ended::Retired);
            }
        }
    }

    let code = match ended {
        Some(Ended::Gone) | None => return,
        Some(Ended::SlowConsumer) => KernelErrorCode::SlowConsumer,
        // The stable set's honest fit: the session is gone, and a retry of
        // the attach now answers with this same code.
        Some(Ended::Retired) => KernelErrorCode::NotFound,
    };
    let last_seq = match delivered.load(Ordering::Acquire) {
        0 => None,
        stored => Some(PtyFrameSeq::new(stored - 1)),
    };
    let _ = responses
        .send(ServerControl::PtyStreamClosed {
            request_id,
            generation,
            code,
            last_seq,
        })
        .await;
}

/// Queue one batch for the write loop. `Err(true)` is a gone connection,
/// `Err(false)` a consumer that stopped reading.
async fn send(
    batches: &mpsc::Sender<Outgoing>,
    request_id: &RequestId,
    session_id: &PtySessionId,
    generation: &PtySessionGeneration,
    batch: PtyBatch,
    delivered: &Arc<AtomicU64>,
) -> Result<(), bool> {
    let outgoing = Outgoing {
        control: ServerControl::PtyDeltaBatch {
            request_id: request_id.clone(),
            session_id: session_id.clone(),
            generation: generation.clone(),
            deltas: batch.deltas.as_ref().clone(),
            seq: PtyFrameSeq::new(batch.seq),
        },
        // `seq + 1`, so revision 0 — a real revision, the engine's first —
        // is distinguishable from "nothing delivered". Saturating: the wire
        // admits `u64::MAX` as a seq, and an overflow panic here would take
        // the connection down over a number.
        delivered: Some((Arc::clone(delivered), batch.seq.saturating_add(1))),
    };
    match tokio::time::timeout(
        std::time::Duration::from_secs(SLOW_CONSUMER_TIMEOUT_SECS),
        batches.send(outgoing),
    )
    .await
    {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(true),
        Err(_) => Err(false),
    }
}

/// The strict decode of a published frame: the refusals the type admits and
/// this layer refuses (the shape rule `gwk_domain::frame::PtyFrame` assigns
/// here), then the expansion into the mirror's grid form. Every refusal
/// lands BEFORE any grid allocation — a ~45-byte `fill` run can name
/// billions of cells, so the walk that judges the frame touches only
/// arithmetic, and the expansion runs on values already proven bounded.
///
/// Refused: a style index outside the table; a duplicate table entry
/// (interning that isn't interning is a lie on the wire); ragged rows; a
/// zero-width run; a dimension past `u16`. Tolerated by the ruled form:
/// adjacent runs sharing a style — a producer should merge, but a merge rule
/// in the decoder buys no correctness and costs every decoder.
fn strict_frame(frame: &PtyFrame) -> Result<(u16, u16, Vec<Vec<StyledCell>>), PtyRefusal> {
    duplicate_free(&frame.styles)?;
    let rows = u16::try_from(frame.rows.len()).map_err(|_| oversized(frame.rows.len()))?;
    let mut width: Option<u64> = None;
    for row in &frame.rows {
        let mut total = 0u64;
        for run in row {
            if run.width() == 0 {
                return Err(PtyRefusal::new(
                    KernelErrorCode::Validation,
                    "a pty frame run must cover at least one cell",
                ));
            }
            if usize::try_from(run.style_index())
                .ok()
                .is_none_or(|index| index >= frame.styles.len())
            {
                return Err(out_of_range(run.style_index(), frame.styles.len()));
            }
            total += run.width() as u64;
        }
        if total > u64::from(u16::MAX) {
            return Err(oversized(total as usize));
        }
        if *width.get_or_insert(total) != total {
            return Err(PtyRefusal::new(
                KernelErrorCode::Validation,
                "a pty frame must be rectangular: every row the same width",
            ));
        }
    }
    let cols = width.unwrap_or(0) as u16;
    // Everything the expansion checks was just refused typed, so this arm is
    // a defensive impossibility, not a second decoder.
    let cells = frame.cells().ok_or_else(|| {
        PtyRefusal::new(
            KernelErrorCode::Validation,
            "a pty frame passed the strict decode and still failed to expand",
        )
    })?;
    Ok((rows, cols, cells))
}

/// The one-canonical-table refusal, shared by the frame and batch decoders.
fn duplicate_free(styles: &[CellStyle]) -> Result<(), PtyRefusal> {
    let mut seen = HashSet::with_capacity(styles.len());
    for style in styles {
        if !seen.insert(style) {
            return Err(PtyRefusal::new(
                KernelErrorCode::Validation,
                "a pty style table carries a duplicate entry — one canonical table only",
            ));
        }
    }
    Ok(())
}

/// The style-index refusal, shared by the frame and batch decoders.
fn out_of_range(index: u32, table: usize) -> PtyRefusal {
    PtyRefusal::new(
        KernelErrorCode::Validation,
        format!("a pty style index {index} is outside its {table}-entry table"),
    )
}

/// Serialized size of a publish, measured once and checked against the
/// budget that guarantees the value can be re-served in one frame.
fn measured<T: serde::Serialize>(value: &T) -> Result<usize, PtyRefusal> {
    let bytes = serde_json::to_vec(value)
        .map_err(|e| PtyRefusal::new(KernelErrorCode::Storage, format!("measure a publish: {e}")))?
        .len();
    if bytes > PUBLISH_BYTE_BUDGET {
        return Err(PtyRefusal::new(
            KernelErrorCode::Validation,
            format!("a publish of {bytes} bytes exceeds the {PUBLISH_BYTE_BUDGET}-byte budget"),
        ));
    }
    Ok(bytes)
}

fn owned(session: &Session, conn: ConnectionId, id: &PtySessionId) -> Result<(), PtyRefusal> {
    if session.owner != conn {
        // `authority`, not `validation`: the request's shape is fine, the
        // ACTOR is not permitted — and a client recovering from each needs to
        // tell them apart by code alone, per the contract's own rule that
        // messages are prose.
        return Err(PtyRefusal::new(
            KernelErrorCode::Authority,
            format!("pty session {id} is claimed by another live connection"),
        ));
    }
    Ok(())
}

fn unknown_session(id: &PtySessionId) -> PtyRefusal {
    PtyRefusal::new(KernelErrorCode::NotFound, format!("no pty session {id}"))
}

/// A non-advancing seq is a CAS refusal — the publisher's view of the head is
/// stale — so it answers `stale_version` with the actual head in the detail,
/// exactly as that code's own doc promises. `validation` would conflate it
/// with a malformed request, which no reconnect could ever fix.
fn backwards(id: &PtySessionId, seq: u64, head: u64) -> PtyRefusal {
    PtyRefusal {
        code: KernelErrorCode::StaleVersion,
        message: format!("pty session {id} is at revision {head}; {seq} does not advance it"),
        detail: Some(serde_json::json!({ "head": head.to_string() })),
    }
}

fn oversized(dimension: usize) -> PtyRefusal {
    PtyRefusal::new(
        KernelErrorCode::Validation,
        format!("a pty grid dimension of {dimension} does not fit u16"),
    )
}

/// The exact serialized size of a `rows`x`cols` all-blank [`PtyFrame`] —
/// what applying a `Resized` delta would put in the mirror — computed by
/// arithmetic so the answer never costs the allocation it exists to refuse.
///
/// Re-derived for the interned shape: a blank grid is one table entry (the
/// blank style, measured once) plus one `fill` run per row (measured once —
/// its `count` field is `cols`, so the digits are right by construction) and
/// JSON punctuation: each row is `[fill]`, the row list is `[row,row,…]`,
/// and the frame wraps both as `{"styles":[s],"rows":…}`. O(rows) bytes, so
/// every practical u16 geometry now seeds trivially — the refusal this
/// feeds keeps its job at the far edge (`u16::MAX`-row screens still bust),
/// and the actual-serialized-bytes rule at publish time remains the real
/// enforcement. The host's declaration check (`gwk-pty-host`'s `publish`
/// module) mirrors this arithmetic.
fn blank_grid_bytes(rows: u16, cols: u16) -> u64 {
    let (rows, cols) = (u64::from(rows), u64::from(cols));
    if rows == 0 {
        // `{"styles":[],"rows":[]}` — no cells, no table entry.
        return 23;
    }
    if cols == 0 {
        // Zero-width rows carry no runs and intern nothing:
        // `{"styles":[],"rows":[[],[],…]}`.
        return 21 + 3 * rows + 1;
    }
    let style = serde_json::to_vec(&blank().style)
        .expect("a blank style serializes")
        .len() as u64;
    let fill = serde_json::to_vec(&PtyRun::Fill {
        style: 0,
        glyph: " ".to_owned(),
        count: cols as u32,
    })
    .expect("a fill run serializes")
    .len() as u64;
    // `{"styles":[` style `],"rows":[` rows x `[` fill `]` joined by commas
    // `]}` — 22 punctuation bytes outside the per-row term.
    (style + 22).saturating_add(rows.saturating_mul(fill + 3))
}

/// A blank cell, matching the host's own convention for a fresh screen: the
/// glyph is a space (the empty string is reserved for a wide cluster's
/// trailing half, per `StyledCell`'s doc).
fn blank() -> StyledCell {
    StyledCell {
        glyph: " ".to_owned(),
        style: CellStyle {
            bold: false,
            dim: false,
            italic: false,
            blink: false,
            inverse: false,
            invisible: false,
            strikethrough: false,
            overline: false,
            underline: None,
            fg: None,
            bg: None,
            underline_color: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gwk_domain::frame::PtyCellUpdate;

    fn id(name: &str) -> PtySessionId {
        PtySessionId::new(name)
    }

    fn frame(rows: u16, cols: u16) -> PtyFrame {
        PtyFrame::from_cells(&vec![vec![blank(); usize::from(cols)]; usize::from(rows)])
    }

    fn update(row: u16, col: u16, glyph: &str) -> PtyCellUpdate {
        // Index 0 into the one-entry blank table `cells_changed` carries.
        PtyCellUpdate {
            row,
            col,
            glyph: glyph.to_owned(),
            style: 0,
        }
    }

    fn cells_changed(updates: Vec<PtyCellUpdate>) -> Vec<PtyDelta> {
        vec![PtyDelta::CellsChanged {
            styles: vec![blank().style],
            updates,
        }]
    }

    fn expanded(frame: &PtyFrame) -> Vec<Vec<StyledCell>> {
        frame.cells().expect("a served snapshot expands")
    }

    #[test]
    fn a_publish_claims_and_a_second_connection_is_refused() {
        let hub = PtyHub::default();
        let (host, stranger) = (hub.connection(), hub.connection());
        hub.publish_snapshot(host, &id("s"), None, frame(2, 4))
            .expect("claim");

        let refused = hub
            .publish_snapshot(stranger, &id("s"), Some(1), frame(2, 4))
            .expect_err("a second publisher must be refused");
        assert_eq!(refused.code, KernelErrorCode::Authority);
        let refused = hub
            .publish_deltas(stranger, &id("s"), 1, cells_changed(vec![]))
            .expect_err("a second publisher must be refused");
        assert_eq!(refused.code, KernelErrorCode::Authority);
        let refused = hub
            .retire(stranger, &id("s"))
            .expect_err("a second publisher must not retire");
        assert_eq!(refused.code, KernelErrorCode::Authority);
    }

    #[test]
    fn revisions_only_move_forward() {
        let hub = PtyHub::default();
        let host = hub.connection();
        hub.publish_snapshot(host, &id("s"), Some(5), frame(2, 4))
            .expect("seed");
        let mut live = hub.attach(&id("s"), None, None).expect("attach").live;

        // Equal is a legal reseed — and a SILENT one: nothing advanced, so
        // nothing is broadcast. Behind is not legal; unsequenced is not.
        hub.publish_snapshot(host, &id("s"), Some(5), frame(2, 4))
            .expect("a reseed at the head is legal");
        assert!(
            live.try_recv().is_err(),
            "a reseed that does not advance the head must not broadcast"
        );
        assert_eq!(
            hub.publish_snapshot(host, &id("s"), Some(4), frame(2, 4))
                .expect_err("behind the head")
                .code,
            KernelErrorCode::StaleVersion
        );
        assert_eq!(
            hub.publish_snapshot(host, &id("s"), None, frame(2, 4))
                .expect_err("unsequenced over sequenced")
                .code,
            KernelErrorCode::StaleVersion
        );
        assert_eq!(
            hub.publish_deltas(host, &id("s"), 5, cells_changed(vec![]))
                .expect_err("a delta must advance the head")
                .code,
            KernelErrorCode::StaleVersion
        );
    }

    #[test]
    fn a_reseed_at_the_head_cannot_change_dimensions() {
        let hub = PtyHub::default();
        let host = hub.connection();
        hub.publish_snapshot(host, &id("s"), Some(5), frame(2, 4))
            .expect("seed");

        // One revision names one screen: consumers holding revision 5 would
        // never hear about new dimensions no seq carries.
        let refused = hub
            .publish_snapshot(host, &id("s"), Some(5), frame(3, 5))
            .expect_err("a non-advancing reseed must keep its dimensions");
        assert_eq!(refused.code, KernelErrorCode::Validation);
        let attached = hub.attach(&id("s"), None, None).expect("attach");
        assert_eq!((attached.rows, attached.cols), (2, 4), "nothing changed");

        // An ADVANCING reseed resizes fine — that is the jump path, and the
        // jump is what broadcasts the resize.
        hub.publish_snapshot(host, &id("s"), Some(6), frame(3, 5))
            .expect("an advancing reseed may resize");
    }

    #[test]
    fn deltas_apply_to_the_mirror_and_snapshots_serve_it() {
        let hub = PtyHub::default();
        let host = hub.connection();
        hub.publish_snapshot(host, &id("s"), Some(0), frame(2, 4))
            .expect("seed");
        hub.publish_deltas(host, &id("s"), 1, cells_changed(vec![update(0, 0, "x")]))
            .expect("apply");

        let (_, seq, snapshot) = hub.snapshot(&id("s")).expect("snapshot");
        assert_eq!(seq, 1);
        let cells = expanded(&snapshot);
        assert_eq!(cells[0][0].glyph, "x");
        assert_eq!(cells[0][1].glyph, " ");
    }

    #[test]
    fn a_batch_with_an_out_of_bounds_update_is_refused_whole() {
        let hub = PtyHub::default();
        let host = hub.connection();
        hub.publish_snapshot(host, &id("s"), Some(0), frame(2, 4))
            .expect("seed");

        // The first update is in bounds; the second is not; neither lands.
        let refused = hub
            .publish_deltas(
                host,
                &id("s"),
                1,
                cells_changed(vec![update(0, 0, "x"), update(9, 0, "y")]),
            )
            .expect_err("out of bounds");
        assert_eq!(refused.code, KernelErrorCode::Validation);
        let (_, seq, snapshot) = hub.snapshot(&id("s")).expect("snapshot");
        assert_eq!(seq, 0, "a refused batch must not advance the head");
        assert_eq!(
            expanded(&snapshot)[0][0].glyph,
            " ",
            "nothing may have landed"
        );

        // A resize inside the batch moves the bounds for the updates after it.
        hub.publish_deltas(
            host,
            &id("s"),
            1,
            vec![
                PtyDelta::Resized { rows: 10, cols: 10 },
                PtyDelta::CellsChanged {
                    styles: vec![blank().style],
                    updates: vec![update(9, 9, "z")],
                },
            ],
        )
        .expect("in bounds after the resize");
        let (_, _, snapshot) = hub.snapshot(&id("s")).expect("snapshot");
        assert_eq!(expanded(&snapshot)[9][9].glyph, "z");
    }

    #[test]
    fn ragged_and_oversized_frames_are_refused() {
        let hub = PtyHub::default();
        let host = hub.connection();
        let mut ragged = frame(2, 4);
        // Rows of width 4 and 3.
        ragged.rows[1] = vec![PtyRun::Fill {
            style: 0,
            glyph: " ".to_owned(),
            count: 3,
        }];
        assert_eq!(
            hub.publish_snapshot(host, &id("s"), None, ragged)
                .expect_err("ragged")
                .code,
            KernelErrorCode::Validation
        );
        let tall = PtyFrame {
            styles: Vec::new(),
            rows: vec![Vec::new(); usize::from(u16::MAX) + 1],
        };
        assert_eq!(
            hub.publish_snapshot(host, &id("s"), None, tall)
                .expect_err("over u16")
                .code,
            KernelErrorCode::Validation
        );
        // A row wider than u16 is the same lie told sideways — and it is a
        // ~50-byte value naming four billion cells, so the refusal must be
        // arithmetic, not a survived allocation.
        let wide = PtyFrame {
            styles: vec![blank().style],
            rows: vec![vec![PtyRun::Fill {
                style: 0,
                glyph: " ".to_owned(),
                count: u32::MAX,
            }]],
        };
        assert_eq!(
            hub.publish_snapshot(host, &id("s"), None, wide)
                .expect_err("a row past u16")
                .code,
            KernelErrorCode::Validation
        );
    }

    #[test]
    fn a_style_index_outside_the_table_is_refused_in_frames_and_batches() {
        let hub = PtyHub::default();
        let host = hub.connection();
        let dangling = PtyFrame {
            styles: vec![blank().style],
            rows: vec![vec![PtyRun::Cells {
                style: 1,
                glyphs: vec!["x".to_owned()],
            }]],
        };
        assert_eq!(
            hub.publish_snapshot(host, &id("s"), None, dangling)
                .expect_err("index 1 into a one-entry table")
                .code,
            KernelErrorCode::Validation
        );

        hub.publish_snapshot(host, &id("s"), Some(0), frame(2, 4))
            .expect("seed");
        let mut orphan = update(0, 0, "x");
        orphan.style = 7;
        let refused = hub
            .publish_deltas(host, &id("s"), 1, cells_changed(vec![orphan]))
            .expect_err("an update indexing past its batch table");
        assert_eq!(refused.code, KernelErrorCode::Validation);
        let (_, seq, _) = hub.snapshot(&id("s")).expect("snapshot");
        assert_eq!(seq, 0, "the refusal must not advance the head");
    }

    #[test]
    fn a_duplicate_style_table_entry_is_refused_in_frames_and_batches() {
        let hub = PtyHub::default();
        let host = hub.connection();
        let twice = PtyFrame {
            styles: vec![blank().style, blank().style],
            rows: vec![vec![PtyRun::Fill {
                style: 0,
                glyph: " ".to_owned(),
                count: 2,
            }]],
        };
        assert_eq!(
            hub.publish_snapshot(host, &id("s"), None, twice)
                .expect_err("interning that isn't interning")
                .code,
            KernelErrorCode::Validation
        );

        hub.publish_snapshot(host, &id("s"), Some(0), frame(2, 4))
            .expect("seed");
        let refused = hub
            .publish_deltas(
                host,
                &id("s"),
                1,
                vec![PtyDelta::CellsChanged {
                    styles: vec![blank().style, blank().style],
                    updates: vec![update(0, 0, "x")],
                }],
            )
            .expect_err("a duplicate batch table");
        assert_eq!(refused.code, KernelErrorCode::Validation);
    }

    #[test]
    fn a_zero_width_run_is_refused_and_non_maximal_runs_are_tolerated() {
        let hub = PtyHub::default();
        let host = hub.connection();
        for empty_run in [
            PtyRun::Fill {
                style: 0,
                glyph: " ".to_owned(),
                count: 0,
            },
            PtyRun::Cells {
                style: 0,
                glyphs: Vec::new(),
            },
        ] {
            let zero = PtyFrame {
                styles: vec![blank().style],
                rows: vec![vec![
                    empty_run,
                    PtyRun::Fill {
                        style: 0,
                        glyph: " ".to_owned(),
                        count: 2,
                    },
                ]],
            };
            assert_eq!(
                hub.publish_snapshot(host, &id("s"), None, zero)
                    .expect_err("a zero-width run")
                    .code,
                KernelErrorCode::Validation
            );
        }

        // The tolerate ruling: two adjacent runs sharing a style are
        // non-canonical, not corrupt — a producer should merge, a decoder
        // that refuses buys no correctness. The split blank row publishes.
        let non_maximal = PtyFrame {
            styles: vec![blank().style],
            rows: vec![vec![
                PtyRun::Fill {
                    style: 0,
                    glyph: " ".to_owned(),
                    count: 1,
                },
                PtyRun::Fill {
                    style: 0,
                    glyph: " ".to_owned(),
                    count: 1,
                },
            ]],
        };
        hub.publish_snapshot(host, &id("s"), None, non_maximal)
            .expect("non-maximal runs are tolerated");
        let attached = hub.attach(&id("s"), None, None).expect("attach");
        assert_eq!((attached.rows, attached.cols), (1, 2));
    }

    #[test]
    fn attach_replays_inside_the_window_and_reseeds_outside_it() {
        let hub = PtyHub::default();
        let host = hub.connection();
        hub.publish_snapshot(host, &id("s"), Some(0), frame(2, 4))
            .expect("seed");
        for seq in 1..=4 {
            hub.publish_deltas(
                host,
                &id("s"),
                seq,
                cells_changed(vec![update(0, 0, &seq.to_string())]),
            )
            .expect("apply");
        }

        let generation = hub
            .attach(&id("s"), None, None)
            .expect("learn the current generation")
            .generation;

        // Inside the window: exactly the batches after the cursor, in order.
        let attached = hub
            .attach(&id("s"), Some(&generation), Some(2))
            .expect("attach");
        assert_eq!(attached.cursor, Some(2));
        assert_eq!(
            attached.replay.iter().map(|b| b.seq).collect::<Vec<_>>(),
            vec![3, 4]
        );
        assert_eq!((attached.rows, attached.cols), (2, 4));

        // Fresh: no replay, resume from the head.
        let attached = hub.attach(&id("s"), None, None).expect("attach");
        assert_eq!(attached.cursor, Some(4));
        assert!(attached.replay.is_empty());

        // Past the head: a revision this life never produced. No replay, and
        // the head answer is what tells the client its cursor was not honored.
        let attached = hub
            .attach(&id("s"), Some(&generation), Some(99))
            .expect("attach");
        assert_eq!(attached.cursor, Some(4));
        assert!(attached.replay.is_empty());

        assert_eq!(
            hub.attach(&id("ghost"), None, None)
                .expect_err("unknown")
                .code,
            KernelErrorCode::NotFound
        );
    }

    #[test]
    fn an_evicted_gap_reseeds_instead_of_replaying_a_hole() {
        let hub = PtyHub::default();
        let host = hub.connection();
        hub.publish_snapshot(host, &id("s"), Some(0), frame(2, 4))
            .expect("seed");
        // Enough batches to evict the head of the window.
        for seq in 1..=(RETAINED_BATCHES as u64 + 8) {
            hub.publish_deltas(host, &id("s"), seq, cells_changed(vec![update(0, 0, "x")]))
                .expect("apply");
        }
        let generation = hub
            .attach(&id("s"), None, None)
            .expect("learn the current generation")
            .generation;
        let attached = hub
            .attach(&id("s"), Some(&generation), Some(1))
            .expect("attach");
        assert_eq!(
            attached.cursor,
            Some(RETAINED_BATCHES as u64 + 8),
            "an evicted cursor is answered at the head"
        );
        assert!(attached.replay.is_empty());
    }

    #[test]
    fn a_snapshot_jump_tells_consumers_their_coordinates_are_void() {
        let hub = PtyHub::default();
        let host = hub.connection();
        hub.publish_snapshot(host, &id("s"), Some(0), frame(2, 4))
            .expect("seed");
        let attached = hub.attach(&id("s"), None, None).expect("attach");
        let mut live = attached.live;

        // The host reseeds past the head with no deltas in between — the
        // consumer cannot get there by applying batches, so the hub says
        // "resize": every held coordinate is void, reseed from a snapshot.
        hub.publish_snapshot(host, &id("s"), Some(10), frame(3, 5))
            .expect("reseed");
        let batch = live.try_recv().expect("the jump must be announced");
        assert_eq!(batch.seq, 10);
        assert!(matches!(
            batch.deltas.as_slice(),
            [PtyDelta::Resized { rows: 3, cols: 5 }]
        ));
    }

    #[test]
    fn retire_and_disconnect_close_the_stream_and_forget_the_id() {
        let hub = PtyHub::default();
        let host = hub.connection();
        hub.publish_snapshot(host, &id("a"), Some(0), frame(2, 4))
            .expect("seed a");
        hub.publish_snapshot(host, &id("b"), Some(0), frame(2, 4))
            .expect("seed b");
        let mut live_a = hub.attach(&id("a"), None, None).expect("attach").live;

        hub.retire(host, &id("a")).expect("retire");
        assert!(matches!(
            live_a.try_recv(),
            Err(broadcast::error::TryRecvError::Closed)
        ));
        assert_eq!(
            hub.snapshot(&id("a")).expect_err("gone").code,
            KernelErrorCode::NotFound
        );

        // The hangup path retires whatever is left.
        hub.disconnect(host);
        assert_eq!(
            hub.snapshot(&id("b")).expect_err("gone").code,
            KernelErrorCode::NotFound
        );

        // And the id is claimable again by a successor connection.
        let successor = hub.connection();
        hub.publish_snapshot(successor, &id("a"), None, frame(1, 1))
            .expect("a successor claims the retired id");
    }

    #[test]
    fn a_session_with_no_frame_yet_has_no_snapshot_but_attaches() {
        let hub = PtyHub::default();
        let host = hub.connection();
        hub.publish_snapshot(host, &id("s"), None, frame(2, 4))
            .expect("announce");
        assert_eq!(
            hub.snapshot(&id("s")).expect_err("no frame yet").code,
            KernelErrorCode::NotFound
        );
        let attached = hub.attach(&id("s"), None, None).expect("attach");
        assert_eq!(attached.cursor, None, "no frame yet: no resume revision");
        assert_eq!((attached.rows, attached.cols), (2, 4));
    }

    #[test]
    fn the_session_count_is_bounded_and_the_refusal_touches_nothing() {
        let hub = PtyHub::default();
        let host = hub.connection();
        for n in 0..MAX_SESSIONS {
            hub.publish_snapshot(host, &id(&format!("s{n}")), None, frame(1, 1))
                .expect("under the cap");
        }
        let refused = hub
            .publish_snapshot(host, &id("one-too-many"), None, frame(1, 1))
            .expect_err("over the cap");
        assert_eq!(refused.code, KernelErrorCode::Overloaded);
        // Existing sessions are untouched, and a reseed of one still works —
        // the cap refuses new claims, not the surface.
        hub.publish_snapshot(host, &id("s0"), Some(1), frame(1, 1))
            .expect("a reseed is not a new claim");
        // Retiring one frees the slot.
        hub.retire(host, &id("s1")).expect("retire");
        hub.publish_snapshot(host, &id("one-too-many"), None, frame(1, 1))
            .expect("a freed slot admits a new claim");
    }

    #[test]
    fn a_reclaimed_id_reseeds_a_stale_cursor_instead_of_faking_continuity() {
        let hub = PtyHub::default();
        let host = hub.connection();
        hub.publish_snapshot(host, &id("s"), Some(0), frame(2, 4))
            .expect("seed");
        for seq in 1..=4 {
            hub.publish_deltas(host, &id("s"), seq, cells_changed(vec![update(0, 0, "x")]))
                .expect("apply");
        }
        let old_generation = hub
            .attach(&id("s"), None, None)
            .expect("learn the first generation")
            .generation;

        // The host's connection dies mid-flight; its successor reclaims the
        // same id at the head its engine meanwhile reached. The reclaim's
        // window is empty: revisions 5..=9 exist and are gone.
        hub.disconnect(host);
        let successor = hub.connection();
        hub.publish_snapshot(successor, &id("s"), Some(9), frame(2, 4))
            .expect("reclaim");

        // A consumer that saw revision 4 of the old life: honoring its
        // cursor would claim gap-free continuity over five silently missing
        // revisions. The head answer is the reseed signal.
        let attached = hub
            .attach(&id("s"), Some(&old_generation), Some(4))
            .expect("attach");
        assert_eq!(attached.cursor, Some(9), "a stale cursor must reseed");
        assert!(attached.replay.is_empty());

        // Once the client holds the new generation, the head is resumable.
        let generation = attached.generation;
        let attached = hub
            .attach(&id("s"), Some(&generation), Some(9))
            .expect("attach");
        assert_eq!(attached.cursor, Some(9));
    }

    #[test]
    fn a_reclaimed_id_changes_generation_when_frame_sequences_overlap() {
        let hub = PtyHub::default();
        let host = hub.connection();
        hub.publish_snapshot(host, &id("s"), Some(4), frame(2, 4))
            .expect("seed the first life");
        let first = hub.attach(&id("s"), None, None).expect("first attach");

        hub.disconnect(host);
        let successor = hub.connection();
        hub.publish_snapshot(successor, &id("s"), Some(4), frame(2, 4))
            .expect("reclaim at the same sequence");
        let reclaimed = hub
            .attach(&id("s"), Some(&first.generation), Some(4))
            .expect("attach to reclaimed life");

        assert_ne!(reclaimed.generation, first.generation);
        assert_eq!(reclaimed.cursor, Some(4));
        assert!(reclaimed.replay.is_empty());
    }

    #[test]
    fn generations_do_not_repeat_across_writer_epochs() {
        let first = PtyHub::new(WriterEpoch::new(7));
        let first_host = first.connection();
        first
            .publish_snapshot(first_host, &id("s"), Some(4), frame(1, 1))
            .expect("first daemon");

        let restarted = PtyHub::new(WriterEpoch::new(8));
        let restarted_host = restarted.connection();
        restarted
            .publish_snapshot(restarted_host, &id("s"), Some(4), frame(1, 1))
            .expect("restarted daemon");

        assert_ne!(
            first
                .attach(&id("s"), None, None)
                .expect("first generation")
                .generation,
            restarted
                .attach(&id("s"), None, None)
                .expect("restarted generation")
                .generation
        );
    }

    #[test]
    fn generation_exhaustion_refuses_without_inserting_a_session() {
        let hub = PtyHub::default();
        hub.generations.store(u64::MAX - 1, Ordering::Relaxed);
        let host = hub.connection();

        hub.publish_snapshot(host, &id("last"), Some(0), frame(1, 1))
            .expect("the final unique generation");
        assert_eq!(
            hub.attach(&id("last"), None, None)
                .expect("last session")
                .generation
                .as_str(),
            format!("{}:{}", hub.writer_epoch, u64::MAX)
        );

        let refused = hub
            .publish_snapshot(host, &id("overflow"), Some(0), frame(1, 1))
            .expect_err("generation reuse must be refused");
        assert_eq!(refused.code, KernelErrorCode::Overloaded);
        assert_eq!(
            hub.attach(&id("overflow"), None, None)
                .expect_err("a refused claim must insert nothing")
                .code,
            KernelErrorCode::NotFound
        );
    }

    #[test]
    fn a_resize_naming_an_unservable_grid_is_refused_before_allocation() {
        let hub = PtyHub::default();
        let host = hub.connection();
        hub.publish_snapshot(host, &id("s"), Some(0), frame(2, 4))
            .expect("seed");

        // The delta is ~40 wire bytes; the mirror it demands is u16::MAX
        // squared cells. The refusal must come from arithmetic, not from
        // surviving the allocation.
        let refused = hub
            .publish_deltas(
                host,
                &id("s"),
                1,
                vec![PtyDelta::Resized {
                    rows: u16::MAX,
                    cols: u16::MAX,
                }],
            )
            .expect_err("a tiny frame must not demand an enormous mirror");
        assert_eq!(refused.code, KernelErrorCode::Validation);
        let (_, seq, snapshot) = hub.snapshot(&id("s")).expect("snapshot");
        assert_eq!(seq, 0, "the refusal must not advance the head");
        assert_eq!(
            expanded(&snapshot).len(),
            2,
            "the refusal must touch nothing"
        );
    }

    #[test]
    fn an_update_glyph_past_the_cell_bound_is_refused() {
        let hub = PtyHub::default();
        let host = hub.connection();
        hub.publish_snapshot(host, &id("s"), Some(0), frame(2, 4))
            .expect("seed");
        let refused = hub
            .publish_deltas(
                host,
                &id("s"),
                1,
                cells_changed(vec![update(0, 0, &"x".repeat(MAX_GLYPH_BYTES + 1))]),
            )
            .expect_err("a glyph past the cell bound must refuse");
        assert_eq!(refused.code, KernelErrorCode::Validation);
        // At the bound is fine — a real grapheme cluster lives far below it.
        hub.publish_deltas(
            host,
            &id("s"),
            1,
            cells_changed(vec![update(0, 0, &"x".repeat(MAX_GLYPH_BYTES))]),
        )
        .expect("at the bound");
    }

    #[test]
    fn a_screen_grown_past_one_frame_refuses_its_snapshot_typed() {
        let hub = PtyHub::default();
        let host = hub.connection();
        // A grid whose BLANK form fits the publish budget…
        hub.publish_snapshot(host, &id("s"), Some(0), frame(150, 100))
            .expect("seed");
        // …grown across batches into per-cell DISTINCT styles — the worst
        // legal case, the one shape interning cannot compress — until the
        // mirror's serialized form no longer fits one response frame. No
        // single publish was the oversized one: each batch carries only its
        // own band's styles in its own table.
        for band in 0..3u16 {
            let mut styles = Vec::new();
            let mut updates = Vec::new();
            for row in 0..50u16 {
                for col in 0..100u16 {
                    let index = styles.len() as u32;
                    let mut style = blank().style;
                    style.fg = Some(gwk_domain::frame::CellColor::Truecolor {
                        r: (band * 50 + row) as u8,
                        g: col as u8,
                        b: 1,
                    });
                    styles.push(style);
                    updates.push(PtyCellUpdate {
                        row: band * 50 + row,
                        col,
                        glyph: "x".to_owned(),
                        style: index,
                    });
                }
            }
            hub.publish_deltas(
                host,
                &id("s"),
                u64::from(band) + 1,
                vec![PtyDelta::CellsChanged { styles, updates }],
            )
            .expect("each batch is inside the publish budget");
        }

        let refused = hub
            .snapshot(&id("s"))
            .expect_err("a mirror past one frame must refuse typed, not kill the connection");
        assert_eq!(refused.code, KernelErrorCode::Overloaded);
        // The recovery the refusal names still works.
        hub.attach(&id("s"), None, None)
            .expect("attach still serves");
    }

    #[test]
    fn blank_grid_arithmetic_matches_real_serialization() {
        // (70, 240) is the phase's deciding geometry: a blank 240x70 screen,
        // 2,536,951 bytes under the per-cell shape, now O(rows).
        for (rows, cols) in [
            (1u16, 1u16),
            (2, 4),
            (24, 80),
            (1, 7),
            (70, 240),
            (0, 0),
            (3, 0),
        ] {
            let actual = serde_json::to_vec(&frame(rows, cols))
                .expect("serialize")
                .len() as u64;
            assert_eq!(
                blank_grid_bytes(rows, cols),
                actual,
                "{rows}x{cols}: the arithmetic must be exact, or the bound drifts from the budget"
            );
        }
    }

    #[tokio::test]
    async fn retire_before_the_first_batch_reports_the_cursor_as_delivered() {
        let hub = PtyHub::default();
        let host = hub.connection();
        hub.publish_snapshot(host, &id("s"), Some(7), frame(1, 1))
            .expect("seed");
        let attached = hub.attach(&id("s"), None, None).expect("attach");
        assert_eq!(attached.cursor, Some(7));

        let (batches, _batches_rx) = mpsc::channel(8);
        let (responses, mut responses_rx) = mpsc::channel(8);
        // The seed the serve layer computes: the attach answer, already seen.
        let delivered = Arc::new(AtomicU64::new(
            attached.cursor.map_or(0, |seq| seq.saturating_add(1)),
        ));
        let pump = tokio::spawn(run_attach(
            RequestId::new("r-1"),
            id("s"),
            attached,
            batches,
            responses,
            delivered,
        ));
        hub.retire(host, &id("s")).expect("retire");
        pump.await.expect("pump");

        match responses_rx.recv().await.expect("a typed close") {
            ServerControl::PtyStreamClosed { code, last_seq, .. } => {
                assert_eq!(code, KernelErrorCode::NotFound);
                assert_eq!(
                    last_seq,
                    Some(PtyFrameSeq::new(7)),
                    "a close before any batch must report the attach cursor, not nothing"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn a_lagged_consumer_is_closed_as_slow() {
        let hub = PtyHub::default();
        let host = hub.connection();
        hub.publish_snapshot(host, &id("s"), Some(0), frame(1, 1))
            .expect("seed");
        let attached = hub.attach(&id("s"), None, None).expect("attach");
        // Flood the broadcast past its capacity before the pump reads a byte.
        for seq in 1..=(BROADCAST_CAPACITY as u64 + 8) {
            hub.publish_deltas(host, &id("s"), seq, cells_changed(vec![update(0, 0, "x")]))
                .expect("apply");
        }

        let (batches, _batches_rx) = mpsc::channel(8);
        let (responses, mut responses_rx) = mpsc::channel(8);
        run_attach(
            RequestId::new("r-1"),
            id("s"),
            attached,
            batches,
            responses,
            Arc::new(AtomicU64::new(1)),
        )
        .await;

        match responses_rx.recv().await.expect("a typed close") {
            ServerControl::PtyStreamClosed { code, last_seq, .. } => {
                assert_eq!(code, KernelErrorCode::SlowConsumer);
                assert_eq!(
                    last_seq,
                    Some(PtyFrameSeq::new(0)),
                    "nothing after the cursor actually left the socket"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_oversized_publish_is_refused_by_bytes() {
        let hub = PtyHub::default();
        let host = hub.connection();
        // One glyph large enough that the serialized frame passes the budget:
        // well-shaped by every strict-decode rule, refused by measurement.
        let big = PtyFrame {
            styles: vec![blank().style],
            rows: vec![vec![PtyRun::Cells {
                style: 0,
                glyphs: vec!["x".repeat(PUBLISH_BYTE_BUDGET + 1)],
            }]],
        };
        assert_eq!(
            hub.publish_snapshot(host, &id("s"), None, big)
                .expect_err("over budget")
                .code,
            KernelErrorCode::Validation
        );
    }

    #[test]
    fn the_retained_window_is_bounded_by_bytes_as_well_as_count() {
        let hub = PtyHub::default();
        let host = hub.connection();
        hub.publish_snapshot(host, &id("s"), Some(0), frame(80, 70))
            .expect("seed");
        // Each batch repaints the whole screen with bound-sized glyphs —
        // ~0.5 MiB serialized under the interned update shape — so sixteen
        // of them cross the byte bound long before the count bound would.
        let glyph = "x".repeat(MAX_GLYPH_BYTES);
        for seq in 1..=16u64 {
            let mut updates = Vec::new();
            for row in 0..80u16 {
                for col in 0..70u16 {
                    updates.push(update(row, col, &glyph));
                }
            }
            hub.publish_deltas(host, &id("s"), seq, cells_changed(updates))
                .expect("apply");
        }
        let generation = hub
            .attach(&id("s"), None, None)
            .expect("learn the current generation")
            .generation;
        let attached = hub
            .attach(&id("s"), Some(&generation), Some(1))
            .expect("attach");
        assert!(
            attached.replay.is_empty(),
            "a cursor behind the byte-evicted horizon must reseed"
        );
        let sessions = hub.lock();
        let session = sessions.get(&id("s")).expect("session");
        assert!(session.retained_bytes <= RETAINED_BYTES);
        assert!(session.evicted_through.is_some());
    }
}
