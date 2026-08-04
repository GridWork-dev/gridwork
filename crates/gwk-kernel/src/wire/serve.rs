//! One connection, from hello to hangup — and the accept loop that owns them.
//!
//! What is served here is the request/response surface: the readiness answers,
//! the one mutation path, and the reads — a projection by id, a projection page,
//! and a page of the log. Every one of them is a question with an answer, which
//! is what makes them one layer: a request goes out, a response comes back on
//! the same `request_id`, and nothing arrives that was not asked for.
//!
//! Subscriptions are the other half, and they are why a connection is two loops
//! instead of one. A batch is a frame the client did not individually request,
//! so it cannot be written by the code that answers requests: one loop owns the
//! write half and takes work from two queues, responses before batches. What a
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

use base64::prelude::{BASE64_STANDARD, Engine as _};
use gwk_domain::blob::BLOB_CHUNK_BYTES;
use gwk_domain::ids::{ByteCount, EventCount, EventId, RequestId, Seq, WriterEpoch};
use gwk_domain::port::{BlobError, BlobStore, EventStore, MAX_READ_LIMIT};
use gwk_domain::protocol::{
    CONNECTION_EGRESS_BYTES_PER_WINDOW, CONNECTION_INGRESS_BYTES_PER_WINDOW, CONTRACT_VERSION,
    FRAME_BODY_MAX_BYTES, FrameKind, KernelErrorCode, KernelRequest, KernelResult,
    MAX_SUBSCRIPTIONS_PER_CONNECTION, ProjectionKind, ProjectionRecord, ServerControl,
};
use sqlx::Row;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, watch};

use super::frame::{Budget, Incoming, read_frame, write_frame};
use super::hello::{self, Readiness};
use super::listen::Listener;
use super::{WireError, strict, subscribe};
use crate::blob::store::PgBlobStore;
use crate::epoch::{
    GENESIS_EVENT_TYPE, KERNEL_AGGREGATE, KERNEL_SINGLETON, epoch_of, is_public_revision,
};
use crate::error::{KernelError, Result};
use crate::store::PgEventStore;

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
    pub(crate) delivered: Option<(Arc<std::sync::atomic::AtomicU64>, u64)>,
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
    /// `subs` is the connection's own state, threaded in because exactly one
    /// request needs it: a subscription is the only answer that outlives the
    /// response carrying it.
    async fn answer(
        &self,
        request_id: &RequestId,
        request: &KernelRequest,
        subs: &mut Subscriptions<'_>,
    ) -> KernelResult {
        match self.try_answer(request_id, request, subs).await {
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
        request_id: &RequestId,
        request: &KernelRequest,
        subs: &mut Subscriptions<'_>,
    ) -> Result<KernelResult> {
        let readiness = self.readiness().await?;
        Ok(match request {
            // Liveness. A sealed kernel is READY — it is serving, it simply
            // admits no business command yet, and conflating the two would make
            // a health check fail every deployment between genesis and cutover.
            KernelRequest::Health {} => KernelResult::Health {
                ready: true,
                sealed: readiness.sealed,
            },
            KernelRequest::Status {} => KernelResult::Status {
                sealed: readiness.sealed,
                watermark: readiness.watermark,
                writer_epoch: self.writer_epoch,
                contract_version: CONTRACT_VERSION,
                public_revision: self.public_revision.clone(),
            },
            KernelRequest::Watermark {} => KernelResult::Watermark {
                watermark: readiness.watermark,
            },
            KernelRequest::VerifySealed {} => self.verify_sealed(readiness.sealed).await?,

            // Forwarded, not gated. `submit` reads the epoch inside its own
            // transaction and under the writer lock, so the sealed allowlist is
            // checked against the state the append will actually commit
            // against. A second check out here would be read at a different
            // instant than the one that matters — it could only ever refuse a
            // command the transaction would have allowed, or wave one through
            // that it then refuses anyway.
            KernelRequest::SubmitCommand { envelope } => self.store.submit(envelope).await,

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
                }
            }

            KernelRequest::ReadEvents { cursor, limit } => {
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

            // The PTY half. This kernel holds no session registry yet — that
            // lands with the pin-advance task that wires the PTY host in.
            // Until then every session id is unknown, which the contract
            // already has an honest answer for: the same refusal
            // `GetProjection`/`BlobStat` give an absent id above, not a new
            // "unimplemented" code invented just for this arm.
            KernelRequest::PtyAttach { session_id, .. } => KernelResult::Error {
                code: KernelErrorCode::NotFound,
                message: format!("no pty session {session_id}"),
                detail: None,
            },
            KernelRequest::PtySnapshot { session_id } => KernelResult::Error {
                code: KernelErrorCode::NotFound,
                message: format!("no pty session {session_id}"),
                detail: None,
            },
        })
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

/// The live subscriptions of one connection, and the queues they write through.
///
/// Per-connection rather than per-daemon because every bound here is about one
/// client's behaviour: how many streams it may hold, and how much the kernel
/// will buffer for it before deciding it has stopped reading.
struct Subscriptions<'a> {
    daemon: &'a Daemon,
    /// Dropping this aborts every subscription on it, which is what makes a
    /// closed connection stop reading the database — a stream parked between
    /// polls has nothing to notice a hangup by.
    live: tokio::task::JoinSet<()>,
    batches: mpsc::Sender<Outgoing>,
    responses: mpsc::Sender<ServerControl>,
    /// An admitted subscription waiting to be started. See [`Self::admit`].
    admitted: Option<(RequestId, Option<Seq>)>,
}

impl<'a> Subscriptions<'a> {
    fn new(
        daemon: &'a Daemon,
        responses: mpsc::Sender<ServerControl>,
        batches: mpsc::Sender<Outgoing>,
    ) -> Self {
        Self {
            daemon,
            live: tokio::task::JoinSet::new(),
            batches,
            responses,
            admitted: None,
        }
    }

    /// Take a subscription request, without starting it.
    ///
    /// Starting is [`Self::start`]'s, one step later, because a subscription's
    /// first batch must not overtake the response that announced it. The task
    /// begins only once `Subscribed` is on the response queue, and the writer
    /// drains that queue first, so the client sees the acknowledgement before
    /// anything acknowledged by it.
    fn admit(&mut self, request_id: &RequestId, cursor: Option<Seq>) -> KernelResult {
        // Ended subscriptions are still in the set until something reaps them,
        // and counting a slow consumer's corpse against a client's allowance
        // would make the cap tighten over the life of a connection.
        while self.live.try_join_next().is_some() {}
        if self.live.len() >= MAX_SUBSCRIPTIONS_PER_CONNECTION {
            // The connection and every stream already on it are untouched: this
            // refuses one request, it does not punish the session.
            return KernelResult::Error {
                code: KernelErrorCode::Overloaded,
                message: format!(
                    "this connection already holds {MAX_SUBSCRIPTIONS_PER_CONNECTION} subscriptions"
                ),
                detail: None,
            };
        }
        self.admitted = Some((request_id.clone(), cursor));
        // Echoes the cursor asked for, which is where the stream starts. A
        // client that sent none gets none back and learns where it is from the
        // first batch.
        KernelResult::Subscribed { cursor }
    }

    /// Start whatever [`Self::admit`] took, now that its response is queued.
    fn start(&mut self) {
        let Some((request_id, cursor)) = self.admitted.take() else {
            return;
        };
        // Seeded with where the stream starts, so a subscription that ends
        // before a single frame left reports its own starting point rather than
        // sending the client back to the beginning of the log.
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
    hello::negotiate(reader, writer, &mut ingress, readiness).await?;

    // One allowance per direction, now that a direction is a loop. They were
    // never coupled — a `spend` only ever touches the counter for the direction
    // it charges — so the split changes nothing but which loop owns which half.
    let mut egress = Budget::new(
        CONNECTION_INGRESS_BYTES_PER_WINDOW,
        CONNECTION_EGRESS_BYTES_PER_WINDOW,
    );
    let (responses_tx, responses_rx) = mpsc::channel(RESPONSE_QUEUE_DEPTH);
    let (batches_tx, batches_rx) = mpsc::channel(BATCH_QUEUE_DEPTH);
    let mut subs = Subscriptions::new(daemon, responses_tx, batches_tx);

    tokio::select! {
        read = read_requests(daemon, reader, &mut ingress, &mut subs) => read,
        written = write_frames(writer, responses_rx, batches_rx, &mut egress) => written,
    }
}

/// Read requests and queue their answers, until the peer hangs up.
async fn read_requests<R>(
    daemon: &Daemon,
    reader: &mut R,
    budget: &mut Budget,
    subs: &mut Subscriptions<'_>,
) -> std::result::Result<(), WireError>
where
    R: AsyncRead + Unpin,
{
    loop {
        let frame = match read_frame(reader, FRAME_BODY_MAX_BYTES, budget).await? {
            Incoming::Frame(frame) => frame,
            Incoming::Closed => return Ok(()),
        };
        if frame.kind != FrameKind::Json {
            return Err(WireError::new(
                KernelErrorCode::Handshake,
                format!("kind {:?} carries no control value", frame.kind),
            ));
        }
        let control: gwk_domain::protocol::ClientControl = strict::decode(&frame.body)?;
        let (request_id, request) = match control {
            gwk_domain::protocol::ClientControl::Request {
                request_id,
                request,
            } => (request_id, request),
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

        let result = daemon.answer(&request_id, &request, subs).await;
        let response = ServerControl::Response { request_id, result };
        if subs.responses.send(response).await.is_err() {
            // The writer is gone, so nothing can be answered any more. Its own
            // error is the one worth reporting and it already returned it.
            return Ok(());
        }
        subs.start();
    }
}

/// Own the write half, and take work from the two queues that feed it.
///
/// Responses first, always. A `StreamClosed` travels on the response queue for
/// this reason: the thing it has to get past is precisely a batch queue that a
/// stopped consumer has left full, and a fair writer would leave the client
/// waiting on the frame that explains why nothing else is coming.
async fn write_frames<W>(
    writer: &mut W,
    mut responses: mpsc::Receiver<ServerControl>,
    mut batches: mpsc::Receiver<Outgoing>,
    budget: &mut Budget,
) -> std::result::Result<(), WireError>
where
    W: AsyncWrite + Unpin,
{
    loop {
        let outgoing = tokio::select! {
            biased;
            Some(control) = responses.recv() => Outgoing { control, delivered: None },
            Some(outgoing) = batches.recv() => outgoing,
            // Both queues closed: the reader is done and no subscription is
            // left to produce anything.
            else => return Ok(()),
        };
        let body = serde_json::to_vec(&outgoing.control).map_err(|e| {
            WireError::new(KernelErrorCode::Storage, format!("serialize a frame: {e}"))
        })?;
        write_frame(writer, FrameKind::Json, &body, budget).await?;
        // AFTER the write, and only on success — the whole value of this number
        // is that nothing is claimed delivered before it left.
        if let Some((cell, seq)) = outgoing.delivered {
            cell.store(seq, std::sync::atomic::Ordering::Release);
        }
    }
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
    // Before the first connection, so a subscription opened on it is on the
    // notified path rather than the poll path from its first read.
    daemon.notify_on_append();

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
}
