//! One connection, from hello to hangup — and the accept loop that owns them.
//!
//! What is served here is the request/response surface: the readiness answers,
//! the one mutation path, and the reads — a projection by id, a projection page,
//! and a page of the log. Every one of them is a question with an answer, which
//! is what makes them one layer: a request goes out, a response comes back on
//! the same `request_id`. The one deliberate reverse control is PTY input: after
//! a send command commits, the kernel routes a typed batch to the connection
//! that owns that hosted session.
//!
//! Subscriptions are the other half, and they are why a connection is two loops
//! instead of one. A batch is a frame the client did not individually request,
//! so it cannot be written by the code that answers requests: one loop owns the
//! write half and takes work from priority queues, responses before reverse
//! controls before batches. What a
//! subscription then DOES lives in [`subscribe`](super::subscribe) — this layer
//! admits one, bounds how many a connection may hold, and writes what they
//! produce.
//!
//! Blob transfer is here too, and it is thinner than it looks: the port already
//! owns staging, digest verification, dedup, and encryption, so what this layer
//! adds is the wire's own concerns — base64 in and out, a bound on a chunk, and
//! a clamp on a read.
//!
//! Pages are cut by BYTES, not rows. The frame limit is the real bound and a row
//! count cannot respect it — see [`PAGE_BYTE_BUDGET`].
//!
//! No wildcard arm anywhere: every request is matched by name, so the compiler
//! has to notice when the protocol grows one.

use std::sync::Arc;

use aead::consts::U16;
use base64::prelude::{BASE64_STANDARD, Engine as _};
use gwk_domain::blob::BLOB_CHUNK_BYTES;
use gwk_domain::command::KernelCommand;
use gwk_domain::ids::{
    ByteCount, EventCount, EventId, PtyFrameSeq, PtySessionGeneration, PtySessionId, RequestId,
    Seq, WriterEpoch,
};
use gwk_domain::port::{BlobError, BlobStore, EventStore, MAX_READ_LIMIT};
use gwk_domain::protocol::{
    CONNECTION_EGRESS_BYTES_PER_WINDOW, CONNECTION_INGRESS_BYTES_PER_WINDOW, CONTRACT_VERSION,
    FRAME_BODY_MAX_BYTES, FRAME_PAYLOAD_MAX_BYTES, FrameKind, KernelErrorCode, KernelRequest,
    KernelResult, MAX_SUBSCRIPTIONS_PER_CONNECTION, PTY_CONTROL_CAPABILITY, PTY_INPUT_CAPABILITY,
    PTY_INPUT_MAX_BASE64_BYTES, PTY_RAW_CAPABILITY, PTY_RAW_PAYLOAD_DEADLINE_SECS,
    PTY_START_CAPABILITY, ProjectionKind, ProjectionRecord, PtyDeliveryResult, PtyInputData,
    ServerControl,
};
use sqlx::Row;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, watch};

use super::frame::{Budget, Frame, Incoming, read_frame, write_frame};
use super::hello::{self, Readiness};
use super::listen::Listener;
use super::pty::{self, PtyHub};
use super::{WireError, receipts, strict, subscribe};
use crate::blob::store::PgBlobStore;
use crate::epoch::{
    GENESIS_EVENT_TYPE, KERNEL_AGGREGATE, KERNEL_SINGLETON, epoch_of, is_public_revision,
};
use crate::error::{KernelError, Result};
use crate::store::PgEventStore;
use crate::submit::PtyInputCarrier;

/// How long shutdown waits for connections that are mid-request.
pub const DRAIN_TIMEOUT_SECS: u64 = 30;

/// Ceiling on one page's serialized items, cursor continuation aside.
///
/// A row count cannot bound a page. An event or an ingested record carries up
/// to `INLINE_PAYLOAD_MAX_BYTES` — 64 KiB — of payload, so sixty-four of them
/// already exceed [`FRAME_BODY_MAX_BYTES`], and the response would fail at the
/// WRITE rather than the request: a fatal framing error, connection closed,
/// over a request that was perfectly well formed. Pages are therefore cut by
/// bytes and the cursor says where. Half the frame leaves room for the response
/// envelope and for re-serialization to land slightly wider than the estimate.
const PAGE_BYTE_BUDGET: usize = (FRAME_BODY_MAX_BYTES as usize) / 2;

/// Rows fetched for a projection page the request gave no limit for.
const DEFAULT_PAGE_ROWS: u32 = 256;

/// Batches the writer may hold for a client that has stopped reading.
///
/// One per subscription the connection is allowed, so a batch that is ready
/// never waits behind another subscription's batch. What it can still wait on is
/// the consumer, which is the only thing the slow-consumer timeout is meant to
/// be measuring.
const BATCH_QUEUE_DEPTH: usize = MAX_SUBSCRIPTIONS_PER_CONNECTION;

/// Responses the writer may hold. One, because the request loop is serial: it
/// reads a request, answers it, and does not read the next until that answer is
/// queued, so a deeper queue would buffer responses that cannot exist.
const RESPONSE_QUEUE_DEPTH: usize = 1;

/// A delivery attempt may spend one slow-consumer window waiting for queue
/// capacity and another waiting for the host's application acknowledgement.
/// The durable lease outlives both, while remaining recoverable after a crash.
const PTY_DELIVERY_LEASE_SECS: u64 = gwk_domain::protocol::SLOW_CONSUMER_TIMEOUT_SECS * 2 + 5;
const PTY_DELIVERY_FAILURE_MAX_BYTES: usize = 4 * 1024;

#[derive(Clone, Copy)]
enum ControlKind {
    Resize,
    Stop,
}

impl ControlKind {
    const fn request_name(self) -> &'static str {
        match self {
            Self::Resize => "resize_pty_session",
            Self::Stop => "stop_pty_session",
        }
    }
}

/// One frame on its way out, and what its departure proves.
///
/// A response proves nothing beyond itself, so `delivered` is `None` for one. A
/// subscription's batch carries the cell its stream reports on closing plus the
/// sequence to publish there — because a batch sitting in the queue has not
/// reached the client, and only the loop that owns the write half can say when
/// one has. See [`subscribe`](super::subscribe) for why that distinction is the
/// difference between a gap-free resume and a silently skipped page.
pub(crate) struct Outgoing {
    pub(crate) control: ServerControl,
    /// A kind `0x02` payload whose `control` is its immediately preceding
    /// header. Kept in one queue item so another stream cannot split the pair.
    pub(crate) raw: Option<Arc<Vec<u8>>>,
    pub(crate) delivered: Option<(Arc<std::sync::atomic::AtomicU64>, u64)>,
    /// A closed stream invalidates anything it left queued. The item already
    /// being written may finish; every later one is skipped before its header.
    pub(crate) active: Option<Arc<std::sync::atomic::AtomicBool>>,
}

/// Cut a page down to what one frame can carry, and say whether it was cut.
///
/// The first item always survives: a page that returned nothing because its
/// leading item was large would make the cursor stand still and the client loop
/// forever. One item cannot overflow a frame on its own — the payload bound is
/// a small fraction of it — so admitting it unconditionally is safe.
///
/// ponytail: serializes each item once here and again in the response frame.
/// Measuring without a second pass means threading a counting writer through
/// the encoder; worth doing if pages ever show up in a profile, not before.
pub(crate) fn fit_page<T: serde::Serialize>(items: Vec<T>) -> Result<(Vec<T>, bool)> {
    let mut kept: Vec<T> = Vec::with_capacity(items.len());
    let mut bytes = 0usize;
    for item in items {
        bytes += serde_json::to_vec(&item)
            .map_err(|e| KernelError::Config(format!("measure a page item: {e}")))?
            .len();
        if !kept.is_empty() && bytes > PAGE_BYTE_BUDGET {
            return Ok((kept, true));
        }
        kept.push(item);
    }
    Ok((kept, false))
}

/// The value a page's next cursor continues from, read out of the last record
/// it delivered.
///
/// `key` is the column the query ordered by, carried here from the same table
/// that holds the SQL — so the cursor a client gets back is by construction the
/// value the next page's `>` compares against. Read off the record rather than
/// selected as a second column, which keeps the read query's record expression
/// byte-identical to the one the checkpoint hashes.
fn cursor_key(record: &ProjectionRecord, key: &str) -> Result<String> {
    let tag = record.kind().as_str();
    let json = serde_json::to_value(record)
        .map_err(|e| KernelError::Config(format!("serialize a {tag} record: {e}")))?;
    json.get(tag)
        .and_then(|body| body.get(key))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| KernelError::Config(format!("a {tag} record has no {key} to page from")))
}

/// What a connection needs to answer the sealed surface.
pub struct Daemon {
    /// Shared rather than owned because a subscription outlives the request that
    /// opened it and reads the log from its own task.
    store: Arc<PgEventStore>,
    public_revision: String,
    writer_epoch: WriterEpoch,
    /// Publishes "the log grew" to every live subscription. Held behind an `Arc`
    /// because the listener that feeds it lives in a task of its own.
    wake: Arc<watch::Sender<u64>>,
    /// The PTY session registry — the one surface answered from memory
    /// rather than the log; see [`pty`]'s module doc for why.
    pty: PtyHub,
}

impl Daemon {
    /// `public_revision` is the build's own 40-hex revision, which `status`
    /// reports so a client can compare a running daemon against the one genesis
    /// recorded. It is required rather than defaulted: a daemon that reported a
    /// placeholder would make that comparison answer "same" for two different
    /// builds.
    ///
    /// The writer epoch is READ OFF the store rather than passed in, because
    /// there is exactly one way to obtain one — `claim_epoch`, which BUMPS the
    /// durable counter — and the store already did it when it opened. A caller
    /// handed this as a parameter would have had to claim a second time, and a
    /// second claim is not a second opinion about the epoch: it supersedes the
    /// first, so the store this daemon is about to serve from would be fenced
    /// out of its own log by the daemon in front of it.
    /// A blob store is likewise required rather than optional. A store without
    /// one is a real state — `admin init` and the certifier drive the log with no
    /// filesystem at all — but a SERVING kernel in that state would answer every
    /// blob request with a refusal that says nothing about the request, and the
    /// operator would find out from a client rather than from a start-up that
    /// failed.
    pub fn new(store: PgEventStore, public_revision: String) -> Result<Self> {
        if !is_public_revision(&public_revision) {
            return Err(KernelError::Config(format!(
                "public revision {public_revision:?} is not a full 40-hex revision"
            )));
        }
        if store.blobs().is_none() {
            return Err(KernelError::Config(
                "a serving daemon needs a blob store: every payload too large to inline lives \
                 there, and half a protocol is not a kernel"
                    .to_owned(),
            ));
        }
        let writer_epoch = WriterEpoch::new(store.boot_epoch().max(0) as u64);
        // The initial receiver is dropped on purpose. A watch with no receivers
        // is a normal state — a daemon nobody has subscribed on — and
        // `send_replace` in the listener treats it as one.
        let (wake, _) = watch::channel(0u64);
        Ok(Self {
            store: Arc::new(store),
            public_revision,
            writer_epoch,
            wake: Arc::new(wake),
            pty: PtyHub::new(writer_epoch),
        })
    }

    /// Start the listener that turns database notifications into wake-ups.
    ///
    /// Separate from [`Daemon::new`] because it spawns, and a constructor that
    /// needed a running runtime to exist would be a trap for any caller that
    /// builds a daemon before starting one. A daemon that never calls this still
    /// serves subscriptions correctly — every one of them re-reads on
    /// `SUBSCRIPTION_POLL_SECS` regardless — at poll latency instead of
    /// notification latency.
    pub fn notify_on_append(&self) {
        tokio::spawn(subscribe::watch_events(
            self.store.pool().clone(),
            Arc::clone(&self.wake),
        ));
    }

    /// Start the periodic TTL sweep: a lease-holder that died rather than
    /// released it gets noticed on [`crate::ttl_sweep::SWEEP_INTERVAL`], not
    /// only on its next renewal attempt. See [`crate::ttl_sweep`].
    ///
    /// Separate from [`Daemon::new`] for the same reason
    /// [`Daemon::notify_on_append`] is: it spawns, and a daemon that never
    /// calls this still serves every other request correctly — dead leases
    /// simply accumulate until something else expires them.
    pub fn spawn_ttl_sweep(&self) {
        tokio::spawn(crate::ttl_sweep::run(
            Arc::clone(&self.store),
            crate::ttl_sweep::SWEEP_INTERVAL,
        ));
    }

    /// A fresh daemon has no in-memory host attempt corresponding to ANY
    /// delivery left pending by an earlier process. Conservatively quarantine
    /// every such row before accepting a connection; recycling it could repeat
    /// an already applied terminal side effect.
    ///
    /// Unclaimed rows are swept too, not just claimed ones. An unclaimed row at
    /// boot is a delivery whose process died between committing the event and
    /// taking the claim — every bit as orphaned as an expired claim, and
    /// nothing else in the running kernel ever clears one.
    async fn quarantine_orphaned_pty_deliveries(&self) -> Result<()> {
        sqlx::query(
            "UPDATE gwk_internal.pty_delivery \
             SET indeterminate_at = now(), claim_token = NULL, claimed_until = NULL \
             WHERE delivered_at IS NULL AND failed_at IS NULL \
               AND indeterminate_at IS NULL",
        )
        .execute(self.store.pool())
        .await
        .map_err(|error| {
            KernelError::Config(format!("quarantine orphaned PTY deliveries: {error}"))
        })?;
        Ok(())
    }

    /// The blob store, whose presence [`Daemon::new`] has already established.
    fn blobs(&self) -> &PgBlobStore {
        self.store
            .blobs()
            .expect("a daemon cannot be constructed without a blob store")
    }

    /// Sealed state and watermark, as the `HelloAck` reports them.
    async fn readiness(&self) -> Result<Readiness> {
        let mut conn = self.connection().await?;
        let epoch = epoch_of(&mut conn)
            .await
            .map_err(|e| KernelError::Config(format!("read the epoch: {e}")))?;
        let watermark = self
            .store
            .watermark()
            .await
            .map_err(|e| KernelError::Config(format!("read the watermark: {e}")))?;
        Ok(Readiness {
            sealed: epoch == crate::epoch::Epoch::Sealed,
            watermark,
        })
    }

    async fn connection(&self) -> Result<sqlx::pool::PoolConnection<sqlx::Postgres>> {
        self.store
            .pool()
            .acquire()
            .await
            .map_err(|e| KernelError::Config(format!("acquire a connection: {e}")))
    }

    /// Answer one request, or say why not. A refusal is a `KernelResult`, never
    /// an error out of band — the connection stays open and the client gets a
    /// value it can branch on.
    ///
    /// `subs` is the connection's own state, threaded in because a
    /// subscription or a PTY attach is an answer that outlives the response
    /// carrying it; `connection` is the identity the PTY hub checks publish
    /// ownership against.
    async fn answer(
        &self,
        connection: pty::ConnectionId,
        request_id: &RequestId,
        request: &KernelRequest,
        subs: &mut Subscriptions<'_>,
    ) -> KernelResult {
        match self.try_answer(connection, request_id, request, subs).await {
            Ok(result) => result,
            Err(e) => KernelResult::Error {
                code: KernelErrorCode::Storage,
                message: e.to_string(),
                detail: None,
            },
        }
    }

    async fn try_answer(
        &self,
        connection: pty::ConnectionId,
        request_id: &RequestId,
        request: &KernelRequest,
        subs: &mut Subscriptions<'_>,
    ) -> Result<KernelResult> {
        // Readiness costs two database round trips, so each arm that consumes
        // it resolves it itself — the PTY family in particular is answered
        // from memory at delta-batch cadence, and a readiness it never reads
        // would tax every keystroke with the log's latency.
        Ok(match request {
            // Liveness. A sealed kernel is READY — it is serving, it simply
            // admits no business command yet, and conflating the two would make
            // a health check fail every deployment between genesis and cutover.
            KernelRequest::Health {} => KernelResult::Health {
                ready: true,
                sealed: self.readiness().await?.sealed,
            },
            KernelRequest::Status {} => {
                let readiness = self.readiness().await?;
                KernelResult::Status {
                    sealed: readiness.sealed,
                    watermark: readiness.watermark,
                    writer_epoch: self.writer_epoch,
                    contract_version: CONTRACT_VERSION,
                    public_revision: self.public_revision.clone(),
                }
            }
            KernelRequest::Watermark {} => KernelResult::Watermark {
                watermark: self.readiness().await?.watermark,
            },
            KernelRequest::VerifySealed {} => {
                let readiness = self.readiness().await?;
                self.verify_sealed(readiness.sealed).await?
            }

            // Forwarded, not gated. `submit` reads the epoch inside its own
            // transaction and under the writer lock, so the sealed allowlist is
            // checked against the state the append will actually commit
            // against. A second check out here would be read at a different
            // instant than the one that matters — it could only ever refuse a
            // command the transaction would have allowed, or wave one through
            // that it then refuses anyway.
            KernelRequest::SubmitCommand { envelope } => {
                match KernelCommand::from_envelope(envelope) {
                    Ok(
                        KernelCommand::ResizePtySession { .. }
                        | KernelCommand::StopPtySession { .. }
                        | KernelCommand::RequestPtySessionStart { .. },
                    ) => KernelResult::Error {
                        code: KernelErrorCode::Validation,
                        message: format!(
                            "{} must use its dedicated PTY request so negotiated capabilities and host delivery cannot be skipped",
                            envelope.command_type
                        ),
                        detail: None,
                    },
                    _ => self.store.submit(envelope).await,
                }
            }
            KernelRequest::SendPtyInput {
                envelope,
                data_base64,
            } => {
                if !subs.input_enabled {
                    KernelResult::Error {
                        code: KernelErrorCode::Capability,
                        message: format!("{PTY_INPUT_CAPABILITY} was not negotiated"),
                        detail: None,
                    }
                } else {
                    self.send_pty_input(connection, envelope, data_base64).await
                }
            }
            KernelRequest::ResizePtySession { envelope } => {
                if !subs.control_enabled {
                    capability_refusal(PTY_CONTROL_CAPABILITY)
                } else {
                    self.send_pty_control(connection, envelope, ControlKind::Resize)
                        .await
                }
            }
            KernelRequest::StopPtySession { envelope } => {
                if !subs.control_enabled {
                    capability_refusal(PTY_CONTROL_CAPABILITY)
                } else {
                    self.send_pty_control(connection, envelope, ControlKind::Stop)
                        .await
                }
            }
            KernelRequest::StartPtySession { envelope } => {
                self.send_pty_start(connection, envelope).await
            }

            KernelRequest::GetProjection { projection, id } => {
                // One row is a one-row page: the same query, asked for an exact
                // key instead of a cursor.
                let (mut records, _, _) =
                    self.projection_page(*projection, None, Some(id), 1).await?;
                match records.pop() {
                    Some(record) => KernelResult::Projection { record },
                    // Absent is NOT an error condition — it is an answer, and
                    // one a caller routinely branches on.
                    None => KernelResult::Error {
                        code: KernelErrorCode::NotFound,
                        message: format!("no {} with id {id:?}", projection.as_str()),
                        detail: None,
                    },
                }
            }

            KernelRequest::ListProjection {
                projection,
                cursor,
                limit,
            } => {
                // Resolved BEFORE the page query: a watermark read after it
                // could land the other side of an append and report the page
                // as fresher than it is.
                let readiness = self.readiness().await?;
                // Clamped, never refused — the same rule the event read
                // documents, so a client that asks for everything gets a page
                // and a cursor rather than a rejection it has to special-case.
                let rows = limit
                    .unwrap_or(DEFAULT_PAGE_ROWS)
                    .clamp(1, MAX_READ_LIMIT as u32);
                let (records, cut, key) = self
                    .projection_page(*projection, cursor.as_deref(), None, rows)
                    .await?;
                // A page that filled either bound may have more behind it. A
                // full page that happened to end the table costs the client one
                // extra empty page; claiming the end early would cost it rows.
                let exhausted = !cut && (records.len() as u32) < rows;
                let next_cursor = if exhausted {
                    None
                } else {
                    records
                        .last()
                        .map(|record| cursor_key(record, key))
                        .transpose()?
                };
                KernelResult::ProjectionPage {
                    records,
                    next_cursor,
                    watermark: readiness.watermark,
                }
            }

            KernelRequest::ReadEvents { cursor, limit } => {
                // Before the event read, for the same page-freshness reason
                // as `ListProjection`.
                let readiness = self.readiness().await?;
                let events = self
                    .store
                    .read_from(*cursor, *limit as usize)
                    .await
                    .map_err(|e| KernelError::Config(format!("read events: {e}")))?;
                // `read_from` bounds the ROW count; this bounds the bytes, which
                // is the bound a frame actually has.
                let (events, _) = fit_page(events)?;
                KernelResult::Events {
                    // The last sequence actually delivered, so a client that
                    // resumes from it resumes from what it received rather than
                    // from what the database was asked for.
                    cursor: events.last().map(|e| e.global_sequence),
                    watermark: readiness.watermark,
                    events,
                }
            }

            // Admitted here, started by the request loop — see
            // [`Subscriptions::admit`] for why the two are separate steps.
            KernelRequest::SubscribeEvents { cursor } => subs.admit(request_id, *cursor),

            // The blob half. Staging, ordering, the declared-size budget, digest
            // verification, dedup, and encryption are all the port's; what is
            // added here is base64, one bound, and one clamp.
            KernelRequest::BlobBegin {
                media_type,
                byte_size,
            } => match self.blobs().begin(media_type.clone(), *byte_size).await {
                Ok(upload_id) => KernelResult::BlobBegun { upload_id },
                Err(e) => blob_refusal(&e),
            },

            KernelRequest::BlobChunk {
                upload_id,
                sequence,
                data_base64,
            } => self.blob_chunk(upload_id, *sequence, data_base64).await,

            KernelRequest::BlobCommit { upload_id, address } => {
                match self
                    .blobs()
                    .commit(upload_id.clone(), address.clone())
                    .await
                {
                    Ok((descriptor, deduplicated)) => KernelResult::BlobCommitted {
                        descriptor,
                        deduplicated,
                    },
                    Err(e) => blob_refusal(&e),
                }
            }

            KernelRequest::BlobAbort { upload_id } => {
                match self.blobs().abort(upload_id.clone()).await {
                    Ok(()) => KernelResult::BlobAborted {
                        upload_id: upload_id.clone(),
                    },
                    Err(e) => blob_refusal(&e),
                }
            }

            KernelRequest::BlobRead {
                address,
                offset,
                length,
            } => {
                // Clamped, not refused — the same rule the log and the
                // projections follow. One chunk is the unit whose base64
                // expansion is guaranteed to fit a frame, and a client that
                // asked for a whole blob gets its head plus the offset to
                // continue from rather than a rejection to special-case.
                let length = ByteCount::new(length.value().min(BLOB_CHUNK_BYTES as u64));
                match self.blobs().read(address, *offset, length).await {
                    Ok(bytes) => KernelResult::BlobBytes {
                        address: address.clone(),
                        // Echoed, so a client reading a blob in pieces never has
                        // to trust its own arithmetic about where this one began.
                        offset: *offset,
                        data_base64: BASE64_STANDARD.encode(&bytes),
                    },
                    Err(e) => blob_refusal(&e),
                }
            }

            KernelRequest::BlobStat { address } => match self.blobs().stat(address).await {
                Ok(Some(descriptor)) => KernelResult::BlobStat { descriptor },
                // Absent is an ANSWER, exactly as it is for a projection. A
                // blob that was SHREDDED is not this case — the port answers
                // that as tombstoned, which is a different fact.
                Ok(None) => KernelResult::Error {
                    code: KernelErrorCode::NotFound,
                    message: format!("no blob at {address}"),
                    detail: None,
                },
                Err(e) => blob_refusal(&e),
            },

            // The PTY half, answered from the hub rather than the log — the
            // session registry the pin-advance comment here used to promise.
            // An attach is admitted like a subscription and started by the
            // request loop, for the same first-batch-after-the-ack ordering.
            KernelRequest::PtyAttach {
                session_id,
                generation,
                cursor,
            } => {
                let result = subs.admit_pty(
                    &self.pty,
                    request_id,
                    session_id,
                    generation.as_ref(),
                    cursor.map(|seq| seq.value()),
                );
                if let KernelResult::PtyAttached { generation, .. } = &result {
                    receipts::attached(&self.store, session_id, generation, connection, request_id)
                        .await;
                }
                result
            }
            KernelRequest::PtyRawAttach {
                session_id,
                generation,
                cursor,
            } => {
                let result = subs.admit_raw(
                    &self.pty,
                    request_id,
                    session_id,
                    generation.as_ref(),
                    cursor.map(|seq| seq.value()),
                );
                if let KernelResult::PtyRawAttached { generation, .. } = &result {
                    receipts::attached(&self.store, session_id, generation, connection, request_id)
                        .await;
                }
                result
            }
            KernelRequest::PtySnapshot { session_id } => match self.pty.snapshot(session_id) {
                Ok((generation, seq, frame)) => KernelResult::PtySnapshot {
                    session_id: session_id.clone(),
                    generation,
                    seq: PtyFrameSeq::new(seq),
                    frame,
                },
                Err(refusal) => refusal.into_result(),
            },
            KernelRequest::PtyPublishSnapshot {
                session_id,
                seq,
                frame,
            } => {
                match self.pty.publish_snapshot(
                    connection,
                    session_id,
                    seq.map(|seq| seq.value()),
                    frame.clone(),
                ) {
                    Ok(opened) => {
                        if let Some(generation) = opened {
                            receipts::opened(&self.store, session_id, &generation).await;
                        }
                        KernelResult::PtyPublished {
                            session_id: session_id.clone(),
                        }
                    }
                    Err(refusal) => refusal.into_result(),
                }
            }
            KernelRequest::PtyPublishDeltas {
                session_id,
                seq,
                deltas,
            } => {
                match self
                    .pty
                    .publish_deltas(connection, session_id, seq.value(), deltas.clone())
                {
                    Ok(()) => KernelResult::PtyPublished {
                        session_id: session_id.clone(),
                    },
                    Err(refusal) => refusal.into_result(),
                }
            }
            KernelRequest::PtyPublishRawResize {
                session_id,
                seq,
                rows,
                cols,
            } => {
                if !subs.raw_enabled {
                    KernelResult::Error {
                        code: KernelErrorCode::Capability,
                        message: format!("{PTY_RAW_CAPABILITY} was not negotiated"),
                        detail: None,
                    }
                } else {
                    match self.pty.publish_raw_resize(
                        connection,
                        session_id,
                        seq.value(),
                        *rows,
                        *cols,
                    ) {
                        Ok(()) => KernelResult::PtyPublished {
                            session_id: session_id.clone(),
                        },
                        Err(refusal) => refusal.into_result(),
                    }
                }
            }
            KernelRequest::PtyRetire { session_id } => {
                match self.pty.retire(connection, session_id) {
                    Ok(generation) => {
                        receipts::closed(&self.store, session_id, &generation, false).await;
                        KernelResult::PtyRetired {
                            session_id: session_id.clone(),
                        }
                    }
                    Err(refusal) => refusal.into_result(),
                }
            }
        })
    }

    /// Commit one metadata-only input command, then enqueue its bytes on the
    /// owning host's existing connection. A retry redelivers only while its
    /// durable delivery row is pending; a settled replay returns the original
    /// result without typing twice.
    async fn send_pty_input(
        &self,
        connection: pty::ConnectionId,
        envelope: &gwk_domain::CommandEnvelope,
        data_base64: &PtyInputData,
    ) -> KernelResult {
        let (session_id, generation, byte_count) = match KernelCommand::from_envelope(envelope) {
            Ok(KernelCommand::SendPtyInput {
                pty_session_id,
                generation,
                byte_count,
            }) => (pty_session_id, generation, byte_count),
            Ok(other) => {
                return KernelResult::Error {
                    code: KernelErrorCode::Validation,
                    message: format!(
                        "the send_pty_input request carries a {} command",
                        other.command_type()
                    ),
                    detail: None,
                };
            }
            Err(error) => {
                return KernelResult::Error {
                    code: KernelErrorCode::Validation,
                    message: error.to_string(),
                    detail: None,
                };
            }
        };
        let decoded = if data_base64.as_str().len() > PTY_INPUT_MAX_BASE64_BYTES {
            Err(format!(
                "PTY input base64 carrier is {} bytes, over the {PTY_INPUT_MAX_BASE64_BYTES}-byte encoded bound",
                data_base64.as_str().len()
            ))
        } else {
            BASE64_STANDARD
                .decode(data_base64.as_str())
                .map_err(|error| format!("PTY input is not valid base64: {error}"))
        };
        let carrier = match &decoded {
            Ok(bytes) => PtyInputCarrier::Bytes(bytes),
            Err(message) => PtyInputCarrier::Invalid(message.clone()),
        };
        let submission = self
            .store
            .submit_pty_input_for_delivery(envelope, carrier)
            .await;
        if !matches!(&submission.result, KernelResult::CommandApplied { .. }) {
            return submission.result;
        }
        let Ok(bytes) = decoded else {
            return KernelResult::Error {
                code: KernelErrorCode::Storage,
                message: "an invalid PTY input carrier was committed".to_owned(),
                detail: Some(serde_json::json!({ "command_committed": true })),
            };
        };

        let canonical = BASE64_STANDARD.encode(&bytes);
        let Some(delivery_id) = submission.delivery_id.clone() else {
            return delivery_storage_error(
                envelope,
                "resolve delivery id",
                "the applied command returned no event",
            );
        };
        self.deliver_submission(connection, submission, envelope, |claim_token| {
            self.pty.deliver_input(
                &session_id,
                &generation,
                claim_token,
                delivery_id,
                envelope.command_id.clone(),
                byte_count,
                PtyInputData::new(canonical),
            )
        })
        .await
    }

    async fn send_pty_control(
        &self,
        connection: pty::ConnectionId,
        envelope: &gwk_domain::CommandEnvelope,
        kind: ControlKind,
    ) -> KernelResult {
        let (session_id, generation, dimensions) =
            match (kind, KernelCommand::from_envelope(envelope)) {
                (
                    ControlKind::Resize,
                    Ok(KernelCommand::ResizePtySession {
                        pty_session_id,
                        generation,
                        cols,
                        rows,
                    }),
                ) => (pty_session_id, generation, Some((cols, rows))),
                (
                    ControlKind::Stop,
                    Ok(KernelCommand::StopPtySession {
                        pty_session_id,
                        generation,
                    }),
                ) => (pty_session_id, generation, None),
                (_, Ok(other)) => return wrong_pty_command(kind.request_name(), &other),
                (_, Err(error)) => return command_decode_refusal(error),
            };
        let submission = self.store.submit_inner_for_delivery(envelope).await;
        let Some(delivery_id) = submission.delivery_id.clone() else {
            return submission.result;
        };
        let control = match dimensions {
            Some((cols, rows)) => ServerControl::PtyResize {
                delivery_id,
                command_id: envelope.command_id.clone(),
                session_id: session_id.clone(),
                generation: generation.clone(),
                cols,
                rows,
            },
            None => ServerControl::PtyStop {
                delivery_id,
                command_id: envelope.command_id.clone(),
                session_id: session_id.clone(),
                generation: generation.clone(),
            },
        };
        self.deliver_submission(connection, submission, envelope, |claim_token| {
            self.pty
                .deliver_control(&session_id, &generation, claim_token, control)
        })
        .await
    }

    async fn send_pty_start(
        &self,
        connection: pty::ConnectionId,
        envelope: &gwk_domain::CommandEnvelope,
    ) -> KernelResult {
        let (template_name, session_id) = match KernelCommand::from_envelope(envelope) {
            Ok(KernelCommand::RequestPtySessionStart {
                template_name,
                pty_session_id,
            }) => (template_name, pty_session_id),
            Ok(other) => return wrong_pty_command("start_pty_session", &other),
            Err(error) => return command_decode_refusal(error),
        };
        let submission = self.store.submit_inner_for_delivery(envelope).await;
        let Some(delivery_id) = submission.delivery_id.clone() else {
            return submission.result;
        };
        let control = ServerControl::PtyStart {
            delivery_id,
            command_id: envelope.command_id.clone(),
            template_name,
            session_id,
        };
        self.deliver_submission(connection, submission, envelope, |claim_token| {
            self.pty.deliver_start(claim_token, control)
        })
        .await
    }

    async fn deliver_submission<F, Fut>(
        &self,
        connection: pty::ConnectionId,
        submission: crate::submit::Submission,
        envelope: &gwk_domain::CommandEnvelope,
        delivery: F,
    ) -> KernelResult
    where
        F: FnOnce(String) -> Fut,
        Fut: std::future::Future<Output = std::result::Result<(), pty::PtyRefusal>>,
    {
        if !matches!(&submission.result, KernelResult::CommandApplied { .. }) {
            return submission.result;
        }
        let Some(delivery_id) = submission.delivery_id.as_ref() else {
            return delivery_storage_error(
                envelope,
                "resolve delivery id",
                "the applied command returned no event",
            );
        };
        if self.pty.delivery_pending(delivery_id) {
            return KernelResult::Error {
                code: KernelErrorCode::Overloaded,
                message: format!(
                    "PTY command {} committed, and another host attempt is still active",
                    envelope.command_id
                ),
                detail: Some(serde_json::json!({
                    "command_committed": true,
                    "command_id": envelope.command_id.as_str(),
                })),
            };
        }
        let nonce = match crate::blob::container::generate::<U16>() {
            Ok(nonce) => crate::blob::container::hex_lower(&nonce),
            Err(error) => return delivery_storage_error(envelope, "mint delivery claim", error),
        };
        let claim_token = format!("{}:{connection}:{nonce}", self.writer_epoch);
        let mut tx = match self.store.pool().begin().await {
            Ok(tx) => tx,
            Err(error) => return delivery_storage_error(envelope, "begin delivery", error),
        };
        let row = match sqlx::query(
            "SELECT delivered_at IS NOT NULL AS delivered, \
                    failed_at IS NOT NULL AS failed, \
                    indeterminate_at IS NOT NULL AS indeterminate, \
                    claim_token IS NOT NULL AS claimed, failure_code, failure_message, \
                    COALESCE(claimed_until > now(), false) AS actively_claimed \
             FROM gwk_internal.pty_delivery \
             WHERE project_id = $1 AND idempotency_key = $2 FOR UPDATE",
        )
        .bind(envelope.project_id.as_str())
        .bind(envelope.idempotency_key.as_str())
        .fetch_optional(&mut *tx)
        .await
        {
            Ok(row) => row,
            Err(error) => return delivery_storage_error(envelope, "lock delivery", error),
        };
        let Some(row) = row else {
            return delivery_storage_error(
                envelope,
                "lock delivery",
                "the committed command has no delivery row",
            );
        };
        let delivered: bool = row.get("delivered");
        if delivered {
            return match tx.commit().await {
                Ok(()) => submission.result,
                Err(error) => delivery_storage_error(envelope, "commit replay", error),
            };
        }
        let failed: bool = row.get("failed");
        if failed {
            let failure_code: String = row.get("failure_code");
            let code = KernelErrorCode::ALL
                .iter()
                .copied()
                .find(|candidate| candidate.as_str() == failure_code)
                .unwrap_or(KernelErrorCode::Storage);
            let message: String = row.get("failure_message");
            if let Err(error) = tx.commit().await {
                return delivery_storage_error(envelope, "commit failed replay", error);
            }
            return KernelResult::Error {
                code,
                message: format!(
                    "PTY command {} committed, but host delivery was terminally refused: {message}",
                    envelope.command_id
                ),
                detail: Some(serde_json::json!({
                    "command_committed": true,
                    "command_id": envelope.command_id.as_str(),
                })),
            };
        }
        let indeterminate: bool = row.get("indeterminate");
        if indeterminate {
            if let Err(error) = tx.commit().await {
                return delivery_storage_error(envelope, "commit indeterminate replay", error);
            }
            return indeterminate_delivery_result(envelope);
        }
        let actively_claimed: bool = row.get("actively_claimed");
        if actively_claimed {
            if let Err(error) = tx.commit().await {
                return delivery_storage_error(envelope, "commit active claim read", error);
            }
            return KernelResult::Error {
                code: KernelErrorCode::Overloaded,
                message: format!(
                    "PTY command {} committed, and another delivery attempt is still active",
                    envelope.command_id
                ),
                detail: Some(serde_json::json!({
                    "command_committed": true,
                    "command_id": envelope.command_id.as_str(),
                })),
            };
        }
        let claimed: bool = row.get("claimed");
        if claimed {
            let updated = match sqlx::query(
                "UPDATE gwk_internal.pty_delivery \
                 SET indeterminate_at = now(), claim_token = NULL, claimed_until = NULL \
                 WHERE project_id = $1 AND idempotency_key = $2 \
                   AND delivered_at IS NULL AND failed_at IS NULL \
                   AND indeterminate_at IS NULL AND claim_token IS NOT NULL",
            )
            .bind(envelope.project_id.as_str())
            .bind(envelope.idempotency_key.as_str())
            .execute(&mut *tx)
            .await
            {
                Ok(updated) => updated,
                Err(error) => {
                    return delivery_storage_error(
                        envelope,
                        "quarantine expired delivery claim",
                        error,
                    );
                }
            };
            if updated.rows_affected() != 1 {
                return delivery_storage_error(
                    envelope,
                    "quarantine expired delivery claim",
                    "the durable claim changed while locked",
                );
            }
            if let Err(error) = tx.commit().await {
                return delivery_storage_error(envelope, "commit indeterminate claim", error);
            }
            return indeterminate_delivery_result(envelope);
        }
        let earlier_pending: bool = match sqlx::query_scalar(
            "SELECT EXISTS (\
               SELECT 1 FROM gwk_internal.pty_delivery d \
               JOIN gwk.event e ON e.event_id = d.event_id \
               JOIN gwk.event current ON current.event_id = (\
                 SELECT event_id FROM gwk_internal.pty_delivery \
                 WHERE project_id = $1 AND idempotency_key = $2\
               ) \
                WHERE d.delivered_at IS NULL AND d.failed_at IS NULL \
                  AND d.indeterminate_at IS NULL \
                 AND d.event_seq < (\
                   SELECT event_seq FROM gwk_internal.pty_delivery \
                   WHERE project_id = $1 AND idempotency_key = $2\
                 ) \
                 AND (\
                   d.claimed_until > now() \
                   OR e.appended_at > now() - make_interval(secs => $3)\
                 ) \
                 AND e.aggregate_type = current.aggregate_type \
                 AND e.aggregate_id = current.aggregate_id\
             )",
        )
        .bind(envelope.project_id.as_str())
        .bind(envelope.idempotency_key.as_str())
        .bind(PTY_DELIVERY_LEASE_SECS as f64)
        .fetch_one(&mut *tx)
        .await
        {
            Ok(pending) => pending,
            Err(error) => return delivery_storage_error(envelope, "check delivery order", error),
        };
        // Only a DELIVERABLE earlier row orders this one. A live claim is an
        // attempt in flight; a freshly appended row is one whose claim is
        // microseconds away in its own request. Anything older with no live
        // claim is abandoned — the process died between the commit and the
        // claim — and ordering against a delivery that will never happen would
        // wedge every later control, keystroke included, for the whole lifetime.
        if earlier_pending {
            return KernelResult::Error {
                code: KernelErrorCode::StaleVersion,
                message: format!(
                    "PTY command {} committed, but an earlier command for the same lifetime is still pending",
                    envelope.command_id
                ),
                detail: Some(serde_json::json!({
                    "command_committed": true,
                    "command_id": envelope.command_id.as_str(),
                })),
            };
        }
        if let Err(error) = sqlx::query(
            "UPDATE gwk_internal.pty_delivery \
             SET claim_token = $3, claimed_until = now() + make_interval(secs => $4) \
              WHERE project_id = $1 AND idempotency_key = $2 \
                AND delivered_at IS NULL AND failed_at IS NULL \
                AND indeterminate_at IS NULL AND claim_token IS NULL",
        )
        .bind(envelope.project_id.as_str())
        .bind(envelope.idempotency_key.as_str())
        .bind(&claim_token)
        .bind(PTY_DELIVERY_LEASE_SECS as f64)
        .execute(&mut *tx)
        .await
        {
            return delivery_storage_error(envelope, "claim delivery", error);
        }
        if let Err(error) = tx.commit().await {
            return delivery_storage_error(envelope, "commit delivery claim", error);
        }
        match delivery(claim_token.clone()).await {
            Ok(()) => submission.result,
            Err(refusal) => {
                if refusal.indeterminate {
                    if let Err(error) =
                        abandon_pty_delivery(&self.store, delivery_id, &claim_token).await
                    {
                        return delivery_storage_error(
                            envelope,
                            "settle indeterminate delivery",
                            error,
                        );
                    }
                    return KernelResult::Error {
                        code: refusal.code,
                        message: format!(
                            "PTY command {} committed, but host application was indeterminate: {}",
                            envelope.command_id, refusal.message
                        ),
                        detail: Some(serde_json::json!({
                            "command_committed": true,
                            "command_id": envelope.command_id.as_str(),
                        })),
                    };
                }
                if let Err(error) = sqlx::query(
                    "UPDATE gwk_internal.pty_delivery \
                     SET claim_token = NULL, claimed_until = NULL \
                      WHERE project_id = $1 AND idempotency_key = $2 \
                        AND claim_token = $3 AND delivered_at IS NULL AND failed_at IS NULL \
                        AND indeterminate_at IS NULL",
                )
                .bind(envelope.project_id.as_str())
                .bind(envelope.idempotency_key.as_str())
                .bind(&claim_token)
                .execute(self.store.pool())
                .await
                {
                    return delivery_storage_error(envelope, "release delivery claim", error);
                }
                KernelResult::Error {
                    code: refusal.code,
                    message: format!(
                        "PTY command {} committed, but host delivery failed: {}",
                        envelope.command_id, refusal.message
                    ),
                    detail: Some(serde_json::json!({
                        "command_committed": true,
                        "command_id": envelope.command_id.as_str(),
                        "delivery_detail": refusal.detail,
                    })),
                }
            }
        }
    }

    async fn settle_pty_delivery(
        &self,
        delivery_id: &EventId,
        claim_token: &str,
        result: &PtyDeliveryResult,
    ) -> Result<()> {
        let updated = match result {
            PtyDeliveryResult::Applied => {
                sqlx::query(
                    "UPDATE gwk_internal.pty_delivery \
                 SET delivered_at = now(), claim_token = NULL, claimed_until = NULL \
                  WHERE event_id = $1 AND delivered_at IS NULL AND failed_at IS NULL \
                     AND indeterminate_at IS NULL \
                     AND claim_token = $2",
                )
                .bind(delivery_id.as_str())
                .bind(claim_token)
                .execute(self.store.pool())
                .await
            }
            PtyDeliveryResult::Refused { code, message } => {
                sqlx::query(
                    "UPDATE gwk_internal.pty_delivery \
                 SET failed_at = now(), failure_code = $3, failure_message = $4, \
                      claim_token = NULL, claimed_until = NULL \
                  WHERE event_id = $1 AND delivered_at IS NULL AND failed_at IS NULL \
                     AND indeterminate_at IS NULL \
                     AND claim_token = $2",
                )
                .bind(delivery_id.as_str())
                .bind(claim_token)
                .bind(code.as_str())
                .bind(message)
                .execute(self.store.pool())
                .await
            }
        }
        .map_err(|error| {
            KernelError::Config(format!("settle PTY delivery {delivery_id}: {error}"))
        })?;
        if updated.rows_affected() != 1 {
            return Err(KernelError::Config(format!(
                "PTY delivery {delivery_id} has no active durable claim"
            )));
        }
        Ok(())
    }

    /// Reconcile an applied acknowledgement retained by a host across a lost
    /// settlement frame or reconnect. Only an already-delivered or terminally
    /// indeterminate row can accept this evidence without racing a live claim.
    async fn reconcile_pty_delivery(
        &self,
        delivery_id: &EventId,
        result: &PtyDeliveryResult,
    ) -> Result<()> {
        if !matches!(result, PtyDeliveryResult::Applied) {
            return Err(KernelError::Config(format!(
                "PTY delivery {delivery_id} has no live attempt for a refusal acknowledgement"
            )));
        }
        let mut tx =
            self.store.pool().begin().await.map_err(|error| {
                KernelError::Config(format!("begin PTY reconciliation: {error}"))
            })?;
        let row = sqlx::query(
            "SELECT delivered_at IS NOT NULL AS delivered, \
                    failed_at IS NOT NULL AS failed, \
                    indeterminate_at IS NOT NULL AS indeterminate, \
                    claim_token IS NOT NULL AS claimed \
             FROM gwk_internal.pty_delivery WHERE event_id = $1 FOR UPDATE",
        )
        .bind(delivery_id.as_str())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|error| KernelError::Config(format!("read PTY reconciliation: {error}")))?
        .ok_or_else(|| KernelError::Config(format!("no PTY delivery {delivery_id}")))?;
        let delivered: bool = row.get("delivered");
        let failed: bool = row.get("failed");
        let indeterminate: bool = row.get("indeterminate");
        let claimed: bool = row.get("claimed");
        if failed || (!delivered && !indeterminate) || claimed {
            return Err(KernelError::Config(format!(
                "PTY delivery {delivery_id} cannot reconcile applied evidence from its current state"
            )));
        }
        if indeterminate {
            let updated = sqlx::query(
                "UPDATE gwk_internal.pty_delivery \
                 SET delivered_at = now(), indeterminate_at = NULL \
                 WHERE event_id = $1 AND indeterminate_at IS NOT NULL \
                   AND delivered_at IS NULL AND failed_at IS NULL AND claim_token IS NULL",
            )
            .bind(delivery_id.as_str())
            .execute(&mut *tx)
            .await
            .map_err(|error| KernelError::Config(format!("write PTY reconciliation: {error}")))?;
            if updated.rows_affected() != 1 {
                return Err(KernelError::Config(format!(
                    "PTY delivery {delivery_id} changed during reconciliation"
                )));
            }
        }
        tx.commit()
            .await
            .map_err(|error| KernelError::Config(format!("commit PTY reconciliation: {error}")))
    }

    async fn abandon_pty_deliveries(&self, deliveries: Vec<(EventId, String)>) {
        for (delivery_id, claim_token) in deliveries {
            if let Err(error) = abandon_pty_delivery(&self.store, &delivery_id, &claim_token).await
            {
                eprintln!("gwk-kernel: abandon PTY delivery {delivery_id}: {error}");
            }
        }
    }

    /// One staged chunk, decoded off the wire.
    async fn blob_chunk(
        &self,
        upload_id: &gwk_domain::ids::BlobUploadId,
        sequence: u32,
        data_base64: &str,
    ) -> KernelResult {
        // Decoded here rather than deeper in: base64 is a WIRE encoding and the
        // port takes bytes. A bad encoding is the client's mistake, not the
        // upload's, so it refuses this request and leaves the upload usable.
        let chunk = match BASE64_STANDARD.decode(data_base64) {
            Ok(chunk) => chunk,
            Err(e) => {
                return KernelResult::Error {
                    code: KernelErrorCode::Validation,
                    message: format!("chunk {sequence} is not valid base64: {e}"),
                    detail: None,
                };
            }
        };
        // The frame limit already bounds this to something a few times larger.
        // Held to the contract's chunk size anyway, so the memory a single
        // request can make the kernel hold is the number the contract states
        // rather than whatever the framing happens to allow.
        if chunk.len() > BLOB_CHUNK_BYTES {
            return KernelResult::Error {
                code: KernelErrorCode::Validation,
                message: format!(
                    "chunk {sequence} carries {} bytes, over the {BLOB_CHUNK_BYTES}-byte chunk",
                    chunk.len()
                ),
                detail: None,
            };
        }
        match self.blobs().write_chunk(upload_id, sequence, &chunk).await {
            Ok(()) => KernelResult::BlobChunkAccepted {
                upload_id: upload_id.clone(),
                sequence,
            },
            Err(e) => blob_refusal(&e),
        }
    }

    /// One page of a projection, as the checkpoint would have canonicalized it.
    ///
    /// `cursor` and `exact` are the two ways to narrow the same query: a page
    /// continues after a key, a get names one. Passing both is meaningless and
    /// no caller does — the SQL would simply AND them.
    async fn projection_page(
        &self,
        kind: ProjectionKind,
        cursor: Option<&str>,
        exact: Option<&str>,
        rows: u32,
    ) -> Result<(Vec<ProjectionRecord>, bool, &'static str)> {
        let (query, key) = crate::checkpoint::read_query(kind).ok_or_else(|| {
            KernelError::Config(format!("projection {} has no table", kind.as_str()))
        })?;
        let mut conn = self.connection().await?;
        let raw: Vec<String> = sqlx::query_scalar(query)
            .bind(cursor)
            .bind(exact)
            .bind(i64::from(rows))
            .fetch_all(&mut *conn)
            .await
            .map_err(|e| KernelError::Config(format!("read {} projections: {e}", kind.as_str())))?;

        let mut records = Vec::with_capacity(raw.len());
        for line in raw {
            // The same round trip the checkpoint makes, and for the same
            // reason: `deny_unknown_fields` turns a column with no contract
            // field into a refusal here, rather than a value that quietly never
            // reaches the client.
            records.push(
                serde_json::from_str::<ProjectionRecord>(&line).map_err(|e| {
                    KernelError::Config(format!(
                        "a {} row does not match the contract type: {e}",
                        kind.as_str()
                    ))
                })?,
            );
        }
        let (records, cut) = fit_page(records)?;
        Ok((records, cut, key))
    }

    /// The fresh-epoch proof: one genesis event, at whatever sequence the
    /// DATABASE gave it, and nothing else in the log.
    async fn verify_sealed(&self, sealed: bool) -> Result<KernelResult> {
        let mut conn = self.connection().await?;
        let row = sqlx::query(
            "SELECT event_id, seq::text AS seq_text FROM gwk.event \
             WHERE aggregate_type = $1 AND aggregate_id = $2 AND event_type = $3 \
             ORDER BY seq LIMIT 1",
        )
        .bind(KERNEL_AGGREGATE)
        .bind(KERNEL_SINGLETON)
        .bind(GENESIS_EVENT_TYPE)
        .fetch_optional(&mut *conn)
        .await
        .map_err(|e| KernelError::Config(format!("read genesis: {e}")))?
        .ok_or_else(|| KernelError::Config("the log has no genesis event".to_owned()))?;

        let event_id: String = row
            .try_get("event_id")
            .map_err(|e| KernelError::Config(format!("genesis event_id: {e}")))?;
        let seq_text: String = row
            .try_get("seq_text")
            .map_err(|e| KernelError::Config(format!("genesis seq: {e}")))?;
        // Never assumed to be 1: the sequence is database-assigned, and a
        // restored or re-created log can legitimately start higher.
        let genesis_watermark = crate::numeric::from_numeric_text(&seq_text)
            .map(Seq::new)
            .map_err(|e| KernelError::Config(format!("genesis seq: {e}")))?;

        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM gwk.event")
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| KernelError::Config(format!("count events: {e}")))?;

        Ok(KernelResult::SealedVerification {
            sealed,
            genesis_event_id: EventId::new(event_id),
            genesis_watermark,
            // A COUNT, unrelated to the sequence above — one is "how many", the
            // other is "which position", and a fresh epoch has count 1 at
            // whatever position the database chose.
            event_count: EventCount::new(count.max(0) as u64),
        })
    }
}

/// One admitted stream waiting to be started — an event subscription or a
/// PTY attach, which share the admission flow because they share its reason:
/// the first unasked-for frame must not overtake the response announcing it.
enum Admitted {
    Events(RequestId, Option<Seq>),
    Pty {
        request_id: RequestId,
        session_id: PtySessionId,
        attached: pty::Attached,
    },
    PtyRaw {
        request_id: RequestId,
        session_id: PtySessionId,
        attached: pty::RawAttached,
    },
}

/// The live streams of one connection, and the queues they write through.
///
/// Per-connection rather than per-daemon because every bound here is about one
/// client's behaviour: how many streams it may hold, and how much the kernel
/// will buffer for it before deciding it has stopped reading. PTY attaches
/// count against the same cap as event subscriptions: each is a cursor, a
/// task, and a queue slot, and a cap that only counted one kind would let a
/// client double its buffered footprint by asking in two vocabularies.
struct Subscriptions<'a> {
    daemon: &'a Daemon,
    /// The connection these streams belong to — receipt keys embed it so two
    /// clients' identically numbered requests stay distinct in the ledger.
    connection: pty::ConnectionId,
    /// Dropping this aborts every stream on it, which is what makes a
    /// closed connection stop reading the database — a stream parked between
    /// polls has nothing to notice a hangup by. Each PTY stream carries a
    /// [`receipts::DetachOnDrop`] guard, so the abort itself is what emits
    /// the detach receipt for a stream that never ended cleanly.
    live: tokio::task::JoinSet<()>,
    batches: mpsc::Sender<Outgoing>,
    responses: mpsc::Sender<ServerControl>,
    /// An admitted stream waiting to be started. See [`Self::admit`].
    admitted: Option<Admitted>,
    raw_enabled: bool,
    input_enabled: bool,
    control_enabled: bool,
}

impl<'a> Subscriptions<'a> {
    /// Take a subscription request, without starting it.
    ///
    /// Starting is [`Self::start`]'s, one step later, because a subscription's
    /// first batch must not overtake the response that announced it. The task
    /// begins only once `Subscribed` is on the response queue, and the writer
    /// drains that queue first, so the client sees the acknowledgement before
    /// anything acknowledged by it.
    fn admit(&mut self, request_id: &RequestId, cursor: Option<Seq>) -> KernelResult {
        if let Some(refusal) = self.full() {
            return refusal;
        }
        self.admitted = Some(Admitted::Events(request_id.clone(), cursor));
        // Echoes the cursor asked for, which is where the stream starts. A
        // client that sent none gets none back and learns where it is from the
        // first batch.
        KernelResult::Subscribed { cursor }
    }

    /// Take a PTY attach, without starting it — the same two-step admission
    /// as an event subscription, for the same ordering reason. The hub is
    /// consulted HERE so the answer can carry the session's dimensions and
    /// the resume revision; what waits for [`Self::start`] is only the
    /// delivery task.
    fn admit_pty(
        &mut self,
        hub: &PtyHub,
        request_id: &RequestId,
        session_id: &PtySessionId,
        generation: Option<&PtySessionGeneration>,
        cursor: Option<u64>,
    ) -> KernelResult {
        if let Some(refusal) = self.full() {
            return refusal;
        }
        let attached = match hub.attach(session_id, generation, cursor) {
            Ok(attached) => attached,
            Err(refusal) => return refusal.into_result(),
        };
        let result = KernelResult::PtyAttached {
            session_id: session_id.clone(),
            generation: attached.generation.clone(),
            rows: attached.rows,
            cols: attached.cols,
            cursor: attached.cursor.map(PtyFrameSeq::new),
        };
        self.admitted = Some(Admitted::Pty {
            request_id: request_id.clone(),
            session_id: session_id.clone(),
            attached,
        });
        result
    }

    fn admit_raw(
        &mut self,
        hub: &PtyHub,
        request_id: &RequestId,
        session_id: &PtySessionId,
        generation: Option<&PtySessionGeneration>,
        cursor: Option<u64>,
    ) -> KernelResult {
        if !self.raw_enabled {
            return KernelResult::Error {
                code: KernelErrorCode::Capability,
                message: format!("{PTY_RAW_CAPABILITY} was not negotiated"),
                detail: None,
            };
        }
        if let Some(refusal) = self.full() {
            return refusal;
        }
        let attached = match hub.attach_raw(session_id, generation, cursor) {
            Ok(attached) => attached,
            Err(refusal) => return refusal.into_result(),
        };
        let result = KernelResult::PtyRawAttached {
            session_id: session_id.clone(),
            generation: attached.generation.clone(),
            rows: attached.rows,
            cols: attached.cols,
            cursor: attached.cursor.map(PtyFrameSeq::new),
        };
        self.admitted = Some(Admitted::PtyRaw {
            request_id: request_id.clone(),
            session_id: session_id.clone(),
            attached,
        });
        result
    }

    /// The cap refusal both admissions share, after reaping ended streams —
    /// counting a slow consumer's corpse against a client's allowance would
    /// make the cap tighten over the life of a connection.
    fn full(&mut self) -> Option<KernelResult> {
        while self.live.try_join_next().is_some() {}
        if self.live.len() < MAX_SUBSCRIPTIONS_PER_CONNECTION {
            return None;
        }
        // The connection and every stream already on it are untouched: this
        // refuses one request, it does not punish the session.
        Some(KernelResult::Error {
            code: KernelErrorCode::Overloaded,
            message: format!(
                "this connection already holds {MAX_SUBSCRIPTIONS_PER_CONNECTION} subscriptions"
            ),
            detail: None,
        })
    }

    /// Start whatever admission took, now that its response is queued.
    fn start(&mut self) {
        match self.admitted.take() {
            None => {}
            Some(Admitted::Events(request_id, cursor)) => {
                // Seeded with where the stream starts, so a subscription that
                // ends before a single frame left reports its own starting
                // point rather than sending the client back to the beginning
                // of the log.
                let delivered = Arc::new(std::sync::atomic::AtomicU64::new(
                    cursor.map_or(0, |seq| seq.value()),
                ));
                self.live.spawn(subscribe::run(
                    Arc::clone(&self.daemon.store),
                    request_id,
                    cursor,
                    self.batches.clone(),
                    self.responses.clone(),
                    self.daemon.wake.subscribe(),
                    delivered,
                ));
            }
            Some(Admitted::Pty {
                request_id,
                session_id,
                attached,
            }) => {
                // Seeded with the resume revision as `seq + 1` (0 = nothing),
                // because revision 0 is real — the engine's first — and the
                // event path's 0-means-none trick would conflate the two.
                let delivered = Arc::new(std::sync::atomic::AtomicU64::new(
                    attached.cursor.map_or(0, |seq| seq.saturating_add(1)),
                ));
                let detach = Some(receipts::DetachOnDrop::new(receipts::DetachReceipt {
                    store: Arc::clone(&self.daemon.store),
                    session_id: session_id.clone(),
                    generation: attached.generation.clone(),
                    connection: self.connection,
                    request_id: request_id.clone(),
                }));
                self.live.spawn(pty::run_attach(
                    request_id,
                    session_id,
                    attached,
                    self.batches.clone(),
                    self.responses.clone(),
                    delivered,
                    detach,
                ));
            }
            Some(Admitted::PtyRaw {
                request_id,
                session_id,
                attached,
            }) => {
                let delivered = Arc::new(std::sync::atomic::AtomicU64::new(
                    attached.delivered.map_or(0, |seq| seq.saturating_add(1)),
                ));
                let active = Arc::new(std::sync::atomic::AtomicBool::new(true));
                let detach = Some(receipts::DetachOnDrop::new(receipts::DetachReceipt {
                    store: Arc::clone(&self.daemon.store),
                    session_id,
                    generation: attached.generation.clone(),
                    connection: self.connection,
                    request_id: request_id.clone(),
                }));
                self.live.spawn(pty::run_raw_attach(
                    request_id,
                    attached,
                    self.batches.clone(),
                    self.responses.clone(),
                    delivered,
                    active,
                    detach,
                ));
            }
        }
    }
}

/// A blob failure as a refusal the client can branch on.
fn blob_refusal(error: &BlobError) -> KernelResult {
    let code = match error {
        BlobError::NotFound => KernelErrorCode::NotFound,
        // Permanent, and answered even while ciphertext cleanup is still
        // pending: a shredded blob never becomes readable again.
        BlobError::Tombstoned => KernelErrorCode::BlobTombstoned,
        // One code for both, because they are the same claim from two sides:
        // the bytes are not the bytes they were said to be.
        BlobError::DigestMismatch { .. } | BlobError::Integrity(_) => {
            KernelErrorCode::BlobIntegrity
        }
        // No request on this wire removes a blob, so a pin cannot block one
        // here. Mapped rather than dismissed, so the match stays exhaustive over
        // the port's errors and a later request that CAN reach it gets a code.
        BlobError::Pinned => KernelErrorCode::Validation,
        BlobError::Storage(_) => KernelErrorCode::Storage,
    };
    KernelResult::Error {
        code,
        // The port's own wording, which already names the address, the sequence,
        // or the two digests. Rephrasing it here would only lose that.
        message: error.to_string(),
        detail: None,
    }
}

fn capability_refusal(capability: &str) -> KernelResult {
    KernelResult::Error {
        code: KernelErrorCode::Capability,
        message: format!("{capability} was not negotiated"),
        detail: None,
    }
}

fn wrong_pty_command(request: &str, command: &KernelCommand) -> KernelResult {
    KernelResult::Error {
        code: KernelErrorCode::Validation,
        message: format!(
            "the {request} request carries a {} command",
            command.command_type()
        ),
        detail: None,
    }
}

fn command_decode_refusal(error: gwk_domain::CommandDecodeError) -> KernelResult {
    KernelResult::Error {
        code: KernelErrorCode::Validation,
        message: error.to_string(),
        detail: None,
    }
}

fn delivery_storage_error(
    envelope: &gwk_domain::CommandEnvelope,
    context: &str,
    error: impl std::fmt::Display,
) -> KernelResult {
    KernelResult::Error {
        code: KernelErrorCode::Storage,
        message: format!(
            "PTY command {} committed, but {context} failed: {error}",
            envelope.command_id
        ),
        detail: Some(serde_json::json!({
            "command_committed": true,
            "command_id": envelope.command_id.as_str(),
        })),
    }
}

fn indeterminate_delivery_result(envelope: &gwk_domain::CommandEnvelope) -> KernelResult {
    KernelResult::Error {
        code: KernelErrorCode::Indeterminate,
        message: format!(
            "PTY command {} committed, but host application could not be determined; refusing unsafe redelivery",
            envelope.command_id
        ),
        detail: Some(serde_json::json!({
            "command_committed": true,
            "command_id": envelope.command_id.as_str(),
        })),
    }
}

fn delivery_ack_result(
    result: &PtyDeliveryResult,
) -> std::result::Result<std::result::Result<(), pty::PtyRefusal>, WireError> {
    Ok(match result {
        PtyDeliveryResult::Applied => Ok(()),
        PtyDeliveryResult::Refused { code, message } => {
            if message.len() > PTY_DELIVERY_FAILURE_MAX_BYTES {
                return Err(WireError::new(
                    KernelErrorCode::FrameSize,
                    format!(
                        "a PTY delivery refusal message is {} bytes; maximum is {PTY_DELIVERY_FAILURE_MAX_BYTES}",
                        message.len()
                    ),
                ));
            }
            Err(pty::PtyRefusal {
                code: *code,
                message: message.clone(),
                detail: None,
                indeterminate: false,
            })
        }
    })
}

async fn abandon_pty_delivery(
    store: &PgEventStore,
    delivery_id: &EventId,
    claim_token: &str,
) -> Result<()> {
    sqlx::query(
        "UPDATE gwk_internal.pty_delivery \
         SET indeterminate_at = now(), claim_token = NULL, claimed_until = NULL \
         WHERE event_id = $1 AND delivered_at IS NULL AND failed_at IS NULL \
           AND indeterminate_at IS NULL AND claim_token = $2",
    )
    .bind(delivery_id.as_str())
    .bind(claim_token)
    .execute(store.pool())
    .await
    .map_err(|error| {
        KernelError::Config(format!(
            "settle abandoned PTY delivery {delivery_id}: {error}"
        ))
    })?;
    Ok(())
}

/// Drive one connection: handshake, then requests until the peer hangs up.
///
/// After the handshake the two directions are driven by two loops, and the first
/// of them to finish ends the connection: the reader finishing means the peer
/// hung up, the writer finishing means the transport failed, and neither is
/// worth continuing half of. Cancelling the reader mid-frame is safe for exactly
/// that reason — nothing is going to read the rest of it.
pub async fn serve_connection<R, W>(
    daemon: &Daemon,
    reader: &mut R,
    writer: &mut W,
) -> std::result::Result<(), WireError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut ingress = Budget::new(
        CONNECTION_INGRESS_BYTES_PER_WINDOW,
        CONNECTION_EGRESS_BYTES_PER_WINDOW,
    );
    let readiness = daemon
        .readiness()
        .await
        .map_err(|e| WireError::new(KernelErrorCode::Storage, format!("readiness: {e}")))?;
    let negotiated = hello::negotiate(reader, writer, &mut ingress, readiness).await?;
    let raw_enabled = negotiated
        .capabilities
        .iter()
        .any(|capability| capability.as_str() == PTY_RAW_CAPABILITY);
    let input_enabled = negotiated
        .capabilities
        .iter()
        .any(|capability| capability.as_str() == PTY_INPUT_CAPABILITY);
    let control_enabled = negotiated
        .capabilities
        .iter()
        .any(|capability| capability.as_str() == PTY_CONTROL_CAPABILITY);
    let start_enabled = negotiated
        .capabilities
        .iter()
        .any(|capability| capability.as_str() == PTY_START_CAPABILITY);

    // One allowance per direction, now that a direction is a loop. They were
    // never coupled — a `spend` only ever touches the counter for the direction
    // it charges — so the split changes nothing but which loop owns which half.
    let mut egress = Budget::new(
        CONNECTION_INGRESS_BYTES_PER_WINDOW,
        CONNECTION_EGRESS_BYTES_PER_WINDOW,
    );
    let (responses_tx, responses_rx) = mpsc::channel(RESPONSE_QUEUE_DEPTH);
    let (inputs_tx, inputs_rx) = mpsc::channel(RESPONSE_QUEUE_DEPTH);
    let (batches_tx, batches_rx) = mpsc::channel(BATCH_QUEUE_DEPTH);

    // The identity PTY publish ownership binds to. Minted per connection and
    // released below whichever way the connection ends, so a host's sessions
    // never outlive the socket that was publishing them.
    let connection = daemon.pty.connection();
    let mut forced_disconnect = daemon.pty.register_connection(
        connection,
        inputs_tx,
        input_enabled,
        control_enabled,
        start_enabled,
    );
    let mut subs = Subscriptions {
        daemon,
        connection,
        live: tokio::task::JoinSet::new(),
        batches: batches_tx,
        responses: responses_tx,
        admitted: None,
        raw_enabled,
        input_enabled,
        control_enabled,
    };
    // Armed BEFORE the request loops: daemon shutdown aborts this whole task,
    // and an aborted future never reaches the inline sweep below — which is
    // how the estate lost a resident session's hangup close at the first
    // kernel restart. The guard makes cancellation itself run the sweep.
    let mut sweep = DisconnectSweep {
        daemon,
        connection,
        armed: true,
    };
    let served = tokio::select! {
        read = read_requests(daemon, connection, reader, &mut ingress, &mut subs) => read,
        written = write_frames(writer, responses_rx, inputs_rx, batches_rx, &mut egress) => written,
        changed = forced_disconnect.changed() => {
            let _ = changed;
            Err(WireError::new(
                KernelErrorCode::SlowConsumer,
                "a PTY host did not acknowledge an applied control within the delivery deadline",
            ))
        },
    };
    sweep.armed = false;
    let pty::DisconnectOutcome { retired, abandoned } = daemon.pty.disconnect(connection);
    daemon.abandon_pty_deliveries(abandoned).await;
    for (id, generation) in retired {
        // The hangup sweep — the `:hangup` provenance is what tells a crash
        // from a typed retire in the cutover receipt.
        receipts::closed(&daemon.store, &id, &generation, true).await;
    }
    served
}

/// The hangup sweep as a drop guard: releases this connection's hosted
/// sessions and hands their close receipts to a task of their own when the
/// connection task is CANCELLED rather than run to its inline sweep. Best
/// effort at full daemon shutdown — a spawned emit can still lose the race
/// against runtime teardown, and the un-closed lifetime row is then the
/// honest crash signature the cutover receipt reads.
struct DisconnectSweep<'a> {
    daemon: &'a Daemon,
    connection: pty::ConnectionId,
    armed: bool,
}

impl Drop for DisconnectSweep<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let pty::DisconnectOutcome {
            retired: swept,
            abandoned,
        } = self.daemon.pty.disconnect(self.connection);
        if swept.is_empty() && abandoned.is_empty() {
            return;
        }
        let store = Arc::clone(&self.daemon.store);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    for (delivery_id, claim_token) in abandoned {
                        if let Err(error) =
                            abandon_pty_delivery(&store, &delivery_id, &claim_token).await
                        {
                            eprintln!("gwk-kernel: abandon PTY delivery {delivery_id}: {error}");
                        }
                    }
                    for (id, generation) in swept {
                        receipts::closed(&store, &id, &generation, true).await;
                    }
                });
            }
            Err(_) => {
                for (delivery_id, _) in abandoned {
                    eprintln!(
                        "gwk-kernel: PTY delivery {delivery_id} disconnected before acknowledgement outside a runtime"
                    );
                }
                for (id, generation) in swept {
                    eprintln!(
                        "gwk-kernel: pty receipt pty-close:{id}:{generation}:hangup: \
                         dropped outside a runtime"
                    );
                }
            }
        }
    }
}

/// A validated JSON header waiting for its immediately following kind `0x02`
/// payload. The request receives exactly one response after the bytes land.
#[derive(Debug)]
enum PendingRawPublish {
    Snapshot {
        request_id: RequestId,
        session_id: PtySessionId,
        seq: Option<u64>,
        rows: u16,
        cols: u16,
        byte_size: usize,
    },
    Output {
        request_id: RequestId,
        session_id: PtySessionId,
        seq: u64,
        byte_size: usize,
    },
}

impl PendingRawPublish {
    fn byte_size(&self) -> usize {
        match self {
            Self::Snapshot { byte_size, .. } | Self::Output { byte_size, .. } => *byte_size,
        }
    }

    fn publish(self, hub: &PtyHub, connection: pty::ConnectionId, bytes: Vec<u8>) -> ServerControl {
        let (request_id, result) = match self {
            Self::Snapshot {
                request_id,
                session_id,
                seq,
                rows,
                cols,
                ..
            } => {
                let result =
                    match hub.publish_raw_snapshot(connection, &session_id, seq, rows, cols, bytes)
                    {
                        Ok(()) => KernelResult::PtyPublished { session_id },
                        Err(refusal) => refusal.into_result(),
                    };
                (request_id, result)
            }
            Self::Output {
                request_id,
                session_id,
                seq,
                ..
            } => {
                let result = match hub.publish_raw_output(connection, &session_id, seq, bytes) {
                    Ok(()) => KernelResult::PtyPublished { session_id },
                    Err(refusal) => refusal.into_result(),
                };
                (request_id, result)
            }
        };
        ServerControl::Response { request_id, result }
    }

    fn accept(self, frame: Frame) -> std::result::Result<(Self, Vec<u8>), WireError> {
        if frame.kind != FrameKind::PtyRaw {
            return Err(WireError::new(
                KernelErrorCode::Handshake,
                "a raw PTY publish header must be followed by kind 0x02",
            ));
        }
        if frame.body.len() != self.byte_size() {
            return Err(WireError::new(
                KernelErrorCode::FrameSize,
                format!(
                    "raw PTY header announced {} bytes but the payload carried {}",
                    self.byte_size(),
                    frame.body.len()
                ),
            ));
        }
        Ok((self, frame.body))
    }
}

fn raw_payload_size(byte_size: ByteCount) -> std::result::Result<usize, WireError> {
    let byte_size = usize::try_from(byte_size.value()).map_err(|_| {
        WireError::new(
            KernelErrorCode::FrameSize,
            "raw PTY byte_size does not fit this host",
        )
    })?;
    if byte_size > FRAME_PAYLOAD_MAX_BYTES {
        return Err(WireError::new(
            KernelErrorCode::FrameSize,
            format!(
                "raw PTY byte_size {byte_size} exceeds the {FRAME_PAYLOAD_MAX_BYTES}-byte payload maximum"
            ),
        ));
    }
    Ok(byte_size)
}

/// Read requests and queue their answers, until the peer hangs up.
async fn read_requests<R>(
    daemon: &Daemon,
    connection: pty::ConnectionId,
    reader: &mut R,
    budget: &mut Budget,
    subs: &mut Subscriptions<'_>,
) -> std::result::Result<(), WireError>
where
    R: AsyncRead + Unpin,
{
    let mut pending_raw: Option<PendingRawPublish> = None;
    loop {
        let frame = match read_request_frame(reader, budget, pending_raw.is_some()).await? {
            Incoming::Frame(frame) => frame,
            Incoming::Closed if pending_raw.is_some() => {
                return Err(WireError::new(
                    KernelErrorCode::FrameSize,
                    "the connection closed before its announced raw PTY payload",
                ));
            }
            Incoming::Closed => return Ok(()),
        };
        if let Some(pending) = pending_raw.take() {
            let (pending, bytes) = pending.accept(frame)?;
            let response = pending.publish(&daemon.pty, connection, bytes);
            if subs.responses.send(response).await.is_err() {
                return Ok(());
            }
            continue;
        }
        if frame.kind != FrameKind::Json {
            return Err(WireError::new(
                KernelErrorCode::Handshake,
                "kind 0x02 arrived without a raw PTY publish header",
            ));
        }
        let control: gwk_domain::protocol::ClientControl = strict::decode(&frame.body)?;
        let (request_id, request) = match control {
            gwk_domain::protocol::ClientControl::PtyDeliveryAck {
                delivery_id,
                result,
            } => {
                let ack_result = delivery_ack_result(&result)?;
                match daemon.pty.delivery_ack_claim(connection, &delivery_id) {
                    Ok(claim_token) => {
                        daemon
                            .settle_pty_delivery(&delivery_id, &claim_token, &result)
                            .await
                            .map_err(|error| {
                                WireError::new(KernelErrorCode::Storage, error.to_string())
                            })?;
                        let sender = daemon
                            .pty
                            .take_delivery_ack(connection, &delivery_id, &claim_token)
                            .map_err(|refusal| WireError::new(refusal.code, refusal.message))?;
                        let _ = sender.send(ack_result);
                    }
                    Err(refusal) if refusal.code == KernelErrorCode::NotFound => {
                        daemon
                            .reconcile_pty_delivery(&delivery_id, &result)
                            .await
                            .map_err(|error| {
                                WireError::new(KernelErrorCode::Storage, error.to_string())
                            })?;
                    }
                    Err(refusal) => {
                        return Err(WireError::new(refusal.code, refusal.message));
                    }
                }
                if subs
                    .responses
                    .send(ServerControl::PtyDeliverySettled { delivery_id })
                    .await
                    .is_err()
                {
                    return Ok(());
                }
                continue;
            }
            gwk_domain::protocol::ClientControl::Request {
                request_id,
                request,
            } => (request_id, request),
            gwk_domain::protocol::ClientControl::PtyRawPublishSnapshot {
                request_id,
                session_id,
                seq,
                rows,
                cols,
                byte_size,
            } => {
                if !subs.raw_enabled {
                    let response = ServerControl::Response {
                        request_id,
                        result: KernelResult::Error {
                            code: KernelErrorCode::Capability,
                            message: format!("{PTY_RAW_CAPABILITY} was not negotiated"),
                            detail: None,
                        },
                    };
                    if subs.responses.send(response).await.is_err() {
                        return Ok(());
                    }
                    continue;
                }
                let byte_size = raw_payload_size(byte_size)?;
                pending_raw = Some(PendingRawPublish::Snapshot {
                    request_id,
                    session_id,
                    seq: seq.map(|seq| seq.value()),
                    rows,
                    cols,
                    byte_size,
                });
                continue;
            }
            gwk_domain::protocol::ClientControl::PtyRawPublishOutput {
                request_id,
                session_id,
                seq,
                byte_size,
            } => {
                if !subs.raw_enabled {
                    let response = ServerControl::Response {
                        request_id,
                        result: KernelResult::Error {
                            code: KernelErrorCode::Capability,
                            message: format!("{PTY_RAW_CAPABILITY} was not negotiated"),
                            detail: None,
                        },
                    };
                    if subs.responses.send(response).await.is_err() {
                        return Ok(());
                    }
                    continue;
                }
                let byte_size = raw_payload_size(byte_size)?;
                pending_raw = Some(PendingRawPublish::Output {
                    request_id,
                    session_id,
                    seq: seq.value(),
                    byte_size,
                });
                continue;
            }
            // A second hello is a client that lost track of its own session.
            // Refused rather than re-negotiated: re-running the handshake
            // mid-connection would let the settled minor and capability set
            // change under requests already in flight.
            gwk_domain::protocol::ClientControl::Hello { .. } => {
                return Err(WireError::new(
                    KernelErrorCode::Handshake,
                    "a second hello on an established connection",
                ));
            }
        };

        let result = daemon.answer(connection, &request_id, &request, subs).await;
        let response = ServerControl::Response { request_id, result };
        if subs.responses.send(response).await.is_err() {
            // The writer is gone, so nothing can be answered any more. Its own
            // error is the one worth reporting and it already returned it.
            return Ok(());
        }
        subs.start();
    }
}

async fn read_request_frame<R>(
    reader: &mut R,
    budget: &mut Budget,
    awaiting_raw_payload: bool,
) -> std::result::Result<Incoming, WireError>
where
    R: AsyncRead + Unpin,
{
    if !awaiting_raw_payload {
        return read_frame(reader, FRAME_BODY_MAX_BYTES, budget).await;
    }
    tokio::time::timeout(
        std::time::Duration::from_secs(PTY_RAW_PAYLOAD_DEADLINE_SECS),
        read_frame(reader, FRAME_BODY_MAX_BYTES, budget),
    )
    .await
    .map_err(|_| {
        WireError::new(
            KernelErrorCode::Handshake,
            format!(
                "a raw PTY publish payload did not follow its header within \
                 {PTY_RAW_PAYLOAD_DEADLINE_SECS}s"
            ),
        )
    })?
}

/// Own the write half, and take work from the three queues that feed it.
///
/// Responses first, always. A `StreamClosed` travels on the response queue for
/// this reason: the thing it has to get past is precisely a batch queue that a
/// stopped consumer has left full, and a fair writer would leave the client
/// waiting on the frame that explains why nothing else is coming.
async fn write_frames<W>(
    writer: &mut W,
    mut responses: mpsc::Receiver<ServerControl>,
    mut inputs: mpsc::Receiver<pty::PtyControl>,
    mut batches: mpsc::Receiver<Outgoing>,
    budget: &mut Budget,
) -> std::result::Result<(), WireError>
where
    W: AsyncWrite + Unpin,
{
    loop {
        let outgoing = tokio::select! {
            biased;
            Some(control) = responses.recv() => Outgoing {
                    control,
                    raw: None,
                    delivered: None,
                    active: None,
                },
            Some(input) = inputs.recv() => Outgoing {
                    control: input.control,
                    raw: None,
                    delivered: None,
                    active: None,
                },
            Some(outgoing) = batches.recv() => outgoing,
            // All queues closed: the reader is done and no subscription is
            // left to produce anything.
            else => return Ok(()),
        };
        if outgoing
            .active
            .as_ref()
            .is_some_and(|active| !active.load(std::sync::atomic::Ordering::Acquire))
        {
            continue;
        }
        let bounded_write = outgoing.active.is_some()
            || matches!(
                outgoing.control,
                ServerControl::PtyInput { .. }
                    | ServerControl::PtyResize { .. }
                    | ServerControl::PtyStop { .. }
                    | ServerControl::PtyStart { .. }
            );
        let written = if bounded_write {
            match tokio::time::timeout(
                std::time::Duration::from_secs(gwk_domain::protocol::SLOW_CONSUMER_TIMEOUT_SECS),
                write_outgoing(writer, &outgoing.control, outgoing.raw.as_deref(), budget),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => {
                    if let Some(active) = &outgoing.active {
                        active.store(false, std::sync::atomic::Ordering::Release);
                    }
                    let error = WireError::new(
                        KernelErrorCode::SlowConsumer,
                        "a PTY control delivery blocked on the client beyond the slow-consumer timeout",
                    );
                    return Err(error);
                }
            }
        } else {
            // Event and rendered-PTY streams enforce backpressure at their
            // bounded queues. Cancelling a framed write here can leave the peer
            // with a length prefix and only part of its body.
            write_outgoing(writer, &outgoing.control, outgoing.raw.as_deref(), budget).await
        };
        written?;
        // AFTER the write, and only on success — the whole value of this number
        // is that nothing is claimed delivered before it left.
        if let Some((cell, seq)) = outgoing.delivered {
            cell.store(seq, std::sync::atomic::Ordering::Release);
        }
    }
}

async fn write_outgoing<W>(
    writer: &mut W,
    control: &ServerControl,
    raw: Option<&Vec<u8>>,
    budget: &mut Budget,
) -> std::result::Result<(), WireError>
where
    W: AsyncWrite + Unpin,
{
    let body = serde_json::to_vec(control)
        .map_err(|e| WireError::new(KernelErrorCode::Storage, format!("serialize a frame: {e}")))?;
    write_frame(writer, FrameKind::Json, &body, budget).await?;
    if let Some(raw) = raw {
        write_frame(writer, FrameKind::PtyRaw, raw, budget).await?;
    }
    Ok(())
}

/// What a clean stop managed to do, for the caller to report.
///
/// The checkpoint is reported rather than returned as an error because a
/// snapshot that fails must not keep the socket on the filesystem or the writer
/// lock held — the next kernel would inherit both. It is reported rather than
/// swallowed because a barrier that has silently stopped firing grows recovery
/// time without bound, and the first anyone hears of it is a restart that
/// replays the whole log.
#[derive(Debug, Default)]
pub struct Stopped {
    /// The sequence the parting snapshot was taken at, if one was.
    pub checkpoint: Option<Seq>,
    /// Why there is no checkpoint, when the reason is a failure rather than
    /// having nothing to snapshot.
    pub checkpoint_error: Option<String>,
}

/// Accept until `shutdown` resolves, then stop accepting, drain, and checkpoint.
///
/// The order is the contract's: acceptance stops FIRST, so no connection starts
/// during the drain, then in-flight work gets at most [`DRAIN_TIMEOUT_SECS`],
/// then the projections are snapshotted at the watermark — after the drain, so
/// the snapshot includes the last append rather than racing it — and only then
/// is the socket removed, and only if it is still the file this process created.
pub async fn run<S>(listener: Listener, daemon: Arc<Daemon>, shutdown: S) -> Result<Stopped>
where
    S: std::future::Future<Output = ()> + Send,
{
    daemon.quarantine_orphaned_pty_deliveries().await?;
    // Before the first connection, so a subscription opened on it is on the
    // notified path rather than the poll path from its first read.
    daemon.notify_on_append();
    daemon.spawn_ttl_sweep();

    let mut connections = tokio::task::JoinSet::new();
    let shutdown = std::pin::pin!(shutdown);
    let mut shutdown = shutdown;

    loop {
        tokio::select! {
            // Biased so a pending shutdown wins a ready connection: on the way
            // down, accepting one more is the wrong answer even when it is
            // available.
            biased;
            () = &mut shutdown => break,
            accepted = listener.accept() => {
                let (stream, _peer) = accepted?;
                let daemon = Arc::clone(&daemon);
                connections.spawn(async move {
                    let (mut reader, mut writer) = tokio::io::split(stream);
                    // A connection's failure is its own: a malformed frame from
                    // one client must not take down the daemon serving the
                    // others.
                    let _ = serve_connection(&daemon, &mut reader, &mut writer).await;
                });
            }
        }
    }

    let drained = tokio::time::timeout(std::time::Duration::from_secs(DRAIN_TIMEOUT_SECS), async {
        while connections.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        // Abandoned rather than waited on forever: the socket still has to come
        // off and the writer lock still has to be released, and a client that
        // stopped reading cannot be allowed to hold either.
        connections.shutdown().await;
    }

    // After the drain: an in-flight append that commits during it belongs in the
    // snapshot, and the barrier this takes would otherwise be waiting on it
    // anyway.
    let mut stopped = Stopped::default();
    match daemon.store.checkpoint_at_watermark().await {
        Ok(at) => stopped.checkpoint = at,
        Err(e) => stopped.checkpoint_error = Some(e.to_string()),
    }

    listener.remove();
    Ok(stopped)
}

/// One `UnixStream`, served. The split is here rather than in
/// [`serve_connection`] so the codec stays testable over any pair of pipes.
pub async fn serve_stream(
    daemon: &Daemon,
    stream: UnixStream,
) -> std::result::Result<(), WireError> {
    let (mut reader, mut writer) = tokio::io::split(stream);
    serve_connection(daemon, &mut reader, &mut writer).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn a_page_is_cut_by_bytes_and_never_to_nothing() {
        // Enough half-kilobyte items to pass the budget several times over.
        let items: Vec<String> = (0..8_000).map(|i| format!("{i:0>512}")).collect();
        let (kept, cut) = fit_page(items).expect("measure");
        assert!(cut, "a page far past the budget was not cut");
        let bytes: usize = kept
            .iter()
            .map(|s| serde_json::to_vec(s).expect("serialize").len())
            .sum();
        // The item that pushed the running total over is not kept, so what
        // survives is inside the budget rather than one item past it.
        assert!(bytes <= PAGE_BYTE_BUDGET, "the kept page is over budget");
        assert!(bytes > PAGE_BYTE_BUDGET / 2, "the cut threw away too much");

        // The first item survives even alone over the budget: a page that
        // returned nothing would leave the cursor standing still and the client
        // asking the same question forever.
        let (kept, cut) = fit_page(vec!["x".repeat(PAGE_BYTE_BUDGET * 2)]).expect("measure");
        assert_eq!(kept.len(), 1);
        assert!(
            !cut,
            "one item is not a cut page — there is nothing behind it"
        );
    }

    #[test]
    fn raw_publish_refuses_an_impossible_size_from_the_header() {
        let error = raw_payload_size(ByteCount::new(FRAME_BODY_MAX_BYTES as u64))
            .expect_err("the kind byte leaves less than the full frame maximum for payload");
        assert_eq!(error.code, KernelErrorCode::FrameSize);
        assert!(error.message.contains("payload maximum"));
    }

    #[test]
    fn a_delivery_refusal_message_is_bounded_before_persistence() {
        let result = PtyDeliveryResult::Refused {
            code: KernelErrorCode::Validation,
            message: "x".repeat(PTY_DELIVERY_FAILURE_MAX_BYTES + 1),
        };
        let error = delivery_ack_result(&result)
            .expect_err("oversized host prose must not reach durable state");
        assert_eq!(error.code, KernelErrorCode::FrameSize);
    }

    #[test]
    fn raw_publish_header_accepts_exactly_one_matching_binary_frame() {
        let pending = || PendingRawPublish::Output {
            request_id: RequestId::new("raw-pair"),
            session_id: PtySessionId::new("pty-1"),
            seq: 1,
            byte_size: 2,
        };
        let (_, bytes) = pending()
            .accept(Frame {
                kind: FrameKind::PtyRaw,
                body: vec![0x00, 0xff],
            })
            .expect("matching raw payload");
        assert_eq!(bytes, [0x00, 0xff]);

        let wrong_kind = pending()
            .accept(Frame {
                kind: FrameKind::Json,
                body: vec![0x00, 0xff],
            })
            .expect_err("JSON cannot stand in for a raw payload");
        assert_eq!(wrong_kind.code, KernelErrorCode::Handshake);

        let wrong_size = pending()
            .accept(Frame {
                kind: FrameKind::PtyRaw,
                body: vec![0x00],
            })
            .expect_err("the payload must match its header's byte count");
        assert_eq!(wrong_size.code, KernelErrorCode::FrameSize);
    }

    #[tokio::test(start_paused = true)]
    async fn raw_publish_payload_has_a_deadline_after_its_header() {
        let (_writer, mut reader) = tokio::io::duplex(64);
        let mut budget = Budget::new(1 << 20, 1 << 20);
        let error = read_request_frame(&mut reader, &mut budget, true)
            .await
            .expect_err("an open peer cannot leave a raw header pending forever");
        assert_eq!(error.code, KernelErrorCode::Handshake);
        assert!(error.message.contains("did not follow"));
    }

    #[tokio::test(start_paused = true)]
    async fn raw_attach_backpressure_bounds_a_deliberately_slow_reader() {
        let hub = PtyHub::default();
        let host = hub.connection();
        let session_id = PtySessionId::new("raw-slow");
        hub.publish_snapshot(
            host,
            &session_id,
            Some(0),
            gwk_domain::frame::PtyFrame::from_cells(
                &[vec![
                    gwk_domain::frame::StyledCell {
                        glyph: " ".to_owned(),
                        style: gwk_domain::frame::CellStyle {
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
                    };
                    1
                ]],
                None,
            ),
        )
        .expect("seed render state");
        hub.publish_raw_snapshot(host, &session_id, Some(0), 1, 1, vec![b's'; 4_096])
            .expect("seed raw state");
        let attached = hub
            .attach_raw(&session_id, None, None)
            .expect("attach raw stream");

        let (responses_tx, responses_rx) = mpsc::channel(1);
        let (batches_tx, batches_rx) = mpsc::channel(BATCH_QUEUE_DEPTH);
        let queue_probe = batches_tx.clone();
        let active = Arc::new(AtomicBool::new(true));
        let delivered = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let pump = tokio::spawn(pty::run_raw_attach(
            RequestId::new("raw-view"),
            attached,
            batches_tx,
            responses_tx,
            Arc::clone(&delivered),
            Arc::clone(&active),
            None,
        ));

        // Deliberately do not poll the batch receiver yet. This is the exact
        // queue-facing shape of a reader that has stopped making progress.
        let offered = BATCH_QUEUE_DEPTH as u64 + 2;
        for seq in 1..=offered {
            hub.publish_raw_output(host, &session_id, seq, vec![b'x'; 1_024])
                .expect("offer output to the raw viewer");
        }
        for _ in 0..100 {
            if queue_probe.capacity() == 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(
            queue_probe.capacity(),
            0,
            "the stalled reader must fill exactly the bounded batch queue"
        );

        tokio::time::advance(std::time::Duration::from_secs(
            gwk_domain::protocol::SLOW_CONSUMER_TIMEOUT_SECS + 1,
        ))
        .await;
        pump.await.expect("the raw pump stops at the timeout");
        assert!(!active.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(delivered.load(std::sync::atomic::Ordering::Acquire), 0);

        hub.attach(&session_id, None, None)
            .expect("raw backpressure must not retire the primary render stream");
        hub.publish_raw_output(host, &session_id, offered + 1, vec![b'y'])
            .expect("raw backpressure must not retire the hosted publisher");

        // Once writing resumes, the priority response gets through and every
        // queued item from the invalidated stream is discarded before its
        // header. The close therefore cannot sit behind the full queue it
        // exists to explain.
        drop(queue_probe);
        let (inputs_tx, inputs_rx) = mpsc::channel(1);
        drop(inputs_tx);
        let mut written = Vec::new();
        let mut budget = Budget::new(1 << 20, 1 << 20);
        write_frames(
            &mut written,
            responses_rx,
            inputs_rx,
            batches_rx,
            &mut budget,
        )
        .await
        .expect("drain the close and discard invalidated items");

        let mut written = std::io::Cursor::new(written);
        let closed = read_frame(&mut written, FRAME_BODY_MAX_BYTES, &mut budget)
            .await
            .expect("read typed close");
        let Incoming::Frame(closed) = closed else {
            panic!("the close was not written");
        };
        assert!(matches!(
            serde_json::from_slice::<ServerControl>(&closed.body).expect("decode close"),
            ServerControl::PtyRawStreamClosed {
                code: KernelErrorCode::SlowConsumer,
                last_seq: None,
                ..
            }
        ));
        assert!(matches!(
            read_frame(&mut written, FRAME_BODY_MAX_BYTES, &mut budget)
                .await
                .expect("read end of drained stream"),
            Incoming::Closed
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn one_raw_delivery_times_out_when_the_socket_write_stalls() {
        let (_responses_tx, responses_rx) = mpsc::channel(1);
        let (_inputs_tx, inputs_rx) = mpsc::channel(1);
        let (batches_tx, batches_rx) = mpsc::channel(BATCH_QUEUE_DEPTH);
        let active = Arc::new(AtomicBool::new(true));
        let delivered = Arc::new(std::sync::atomic::AtomicU64::new(0));
        batches_tx
            .send(Outgoing {
                control: ServerControl::PtyRawSnapshot {
                    request_id: RequestId::new("raw-stalled"),
                    generation: PtySessionGeneration::new("life-1"),
                    seq: Some(PtyFrameSeq::new(0)),
                    rows: 24,
                    cols: 80,
                    byte_size: ByteCount::new(4_096),
                },
                raw: Some(Arc::new(vec![b'x'; 4_096])),
                delivered: Some((Arc::clone(&delivered), 1)),
                active: Some(Arc::clone(&active)),
            })
            .await
            .expect("queue raw snapshot");
        drop(batches_tx);

        // Neither half reads. Sixty-four bytes cannot hold the JSON header and
        // payload pair, so the writer itself must enforce the slow-reader bound.
        let (_slow_reader, mut server_writer) = tokio::io::duplex(64);
        let writer = tokio::spawn(async move {
            let mut budget = Budget::new(1 << 20, 1 << 20);
            write_frames(
                &mut server_writer,
                responses_rx,
                inputs_rx,
                batches_rx,
                &mut budget,
            )
            .await
        });
        let guard = tokio::spawn(async move {
            tokio::time::timeout(
                std::time::Duration::from_secs(
                    gwk_domain::protocol::SLOW_CONSUMER_TIMEOUT_SECS + 2,
                ),
                writer,
            )
            .await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(std::time::Duration::from_secs(
            gwk_domain::protocol::SLOW_CONSUMER_TIMEOUT_SECS + 1,
        ))
        .await;
        tokio::task::yield_now().await;

        let error = guard
            .await
            .expect("join guard")
            .expect("the raw writer must stop before the outer guard")
            .expect("join writer")
            .expect_err("a stalled raw write must close the connection");
        assert_eq!(error.code, KernelErrorCode::SlowConsumer);
        assert!(!active.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(delivered.load(std::sync::atomic::Ordering::Acquire), 0);
    }
}
