//! One connection, from hello to hangup — and the accept loop that owns them.
//!
//! What is served here is the request/response surface: the readiness answers,
//! the one mutation path, and the reads — a projection by id, a projection page,
//! and a page of the log. Every one of them is a question with an answer, which
//! is what makes them one layer: a request goes out, a response comes back on
//! the same `request_id`, and nothing arrives that was not asked for.
//!
//! Subscriptions and blob transfer are the other half and are deliberately not
//! here. Both push frames the client did not individually request, which needs a
//! writer that is not the request loop, per-connection queues, and a policy for
//! a consumer that stops reading. That is a different shape, not more of this
//! one.
//!
//! Pages are cut by BYTES, not rows. The frame limit is the real bound and a row
//! count cannot respect it — see [`PAGE_BYTE_BUDGET`].
//!
//! The requests this layer does not answer are matched BY NAME and refused, not
//! swept up by a wildcard: the compiler has to notice when the protocol grows a
//! request, and a reader has to be able to see which ones are still missing.

use std::sync::Arc;

use gwk_domain::ids::{EventCount, EventId, Seq, WriterEpoch};
use gwk_domain::port::{EventStore, MAX_READ_LIMIT};
use gwk_domain::protocol::{
    CONNECTION_EGRESS_MAX_BYTES, CONNECTION_INGRESS_MAX_BYTES, CONTRACT_VERSION,
    FRAME_BODY_MAX_BYTES, FrameKind, KernelErrorCode, KernelRequest, KernelResult, ProjectionKind,
    ProjectionRecord, ServerControl,
};
use sqlx::Row;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;

use super::frame::{Budget, Incoming, read_frame, write_frame};
use super::hello::{self, Readiness};
use super::listen::Listener;
use super::{WireError, strict};
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
fn fit_page<T: serde::Serialize>(items: Vec<T>) -> Result<(Vec<T>, bool)> {
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
    store: PgEventStore,
    public_revision: String,
    writer_epoch: WriterEpoch,
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
    pub fn new(store: PgEventStore, public_revision: String) -> Result<Self> {
        if !is_public_revision(&public_revision) {
            return Err(KernelError::Config(format!(
                "public revision {public_revision:?} is not a full 40-hex revision"
            )));
        }
        let writer_epoch = WriterEpoch::new(store.boot_epoch().max(0) as u64);
        Ok(Self {
            store,
            public_revision,
            writer_epoch,
        })
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
    async fn answer(&self, request: &KernelRequest) -> KernelResult {
        match self.try_answer(request).await {
            Ok(result) => result,
            Err(e) => KernelResult::Error {
                code: KernelErrorCode::Storage,
                message: e.to_string(),
                detail: None,
            },
        }
    }

    async fn try_answer(&self, request: &KernelRequest) -> Result<KernelResult> {
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

            // Named one by one so the protocol cannot grow a request this
            // layer silently drops. The streaming and blob halves of task 19
            // fill these in; until they do the refusal says which request it
            // was, not "unsupported".
            KernelRequest::SubscribeEvents { .. }
            | KernelRequest::BlobBegin { .. }
            | KernelRequest::BlobChunk { .. }
            | KernelRequest::BlobCommit { .. }
            | KernelRequest::BlobAbort { .. }
            | KernelRequest::BlobRead { .. }
            | KernelRequest::BlobStat { .. } => KernelResult::Error {
                code: KernelErrorCode::Validation,
                message: format!(
                    "{} is not served by this daemon's request layer yet",
                    request_name(request)
                ),
                detail: None,
            },
        })
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
        // restored or re-created log can legitimately start higher (ADR 0026).
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

/// The wire name of a request, for a message a human reads.
fn request_name(request: &KernelRequest) -> &'static str {
    match request {
        KernelRequest::Health {} => "health",
        KernelRequest::Status {} => "status",
        KernelRequest::Watermark {} => "watermark",
        KernelRequest::VerifySealed {} => "verify_sealed",
        KernelRequest::SubmitCommand { .. } => "submit_command",
        KernelRequest::GetProjection { .. } => "get_projection",
        KernelRequest::ListProjection { .. } => "list_projection",
        KernelRequest::ReadEvents { .. } => "read_events",
        KernelRequest::SubscribeEvents { .. } => "subscribe_events",
        KernelRequest::BlobBegin { .. } => "blob_begin",
        KernelRequest::BlobChunk { .. } => "blob_chunk",
        KernelRequest::BlobCommit { .. } => "blob_commit",
        KernelRequest::BlobAbort { .. } => "blob_abort",
        KernelRequest::BlobRead { .. } => "blob_read",
        KernelRequest::BlobStat { .. } => "blob_stat",
    }
}

/// Drive one connection: handshake, then requests until the peer hangs up.
pub async fn serve_connection<R, W>(
    daemon: &Daemon,
    reader: &mut R,
    writer: &mut W,
) -> std::result::Result<(), WireError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut budget = Budget::new(CONNECTION_INGRESS_MAX_BYTES, CONNECTION_EGRESS_MAX_BYTES);
    let readiness = daemon
        .readiness()
        .await
        .map_err(|e| WireError::new(KernelErrorCode::Storage, format!("readiness: {e}")))?;
    hello::negotiate(reader, writer, &mut budget, readiness).await?;

    loop {
        let frame = match read_frame(reader, FRAME_BODY_MAX_BYTES, &mut budget).await? {
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

        let response = ServerControl::Response {
            request_id,
            result: daemon.answer(&request).await,
        };
        let body = serde_json::to_vec(&response).map_err(|e| {
            WireError::new(KernelErrorCode::Storage, format!("serialize response: {e}"))
        })?;
        write_frame(writer, FrameKind::Json, &body, &mut budget).await?;
    }
}

/// Accept until `shutdown` resolves, then stop accepting and drain.
///
/// The order is the contract's: acceptance stops FIRST, so no connection starts
/// during the drain, then in-flight work gets at most
/// [`DRAIN_TIMEOUT_SECS`], then the socket is removed — and only if it is still
/// the file this process created.
pub async fn run<S>(listener: Listener, daemon: Arc<Daemon>, shutdown: S) -> Result<()>
where
    S: std::future::Future<Output = ()> + Send,
{
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
    listener.remove();
    Ok(())
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
    fn every_request_has_a_name_and_the_compiler_holds_that_true() {
        // The list exists so a refusal can say WHICH request it refused. It is
        // exhaustive by construction — `request_name` has no wildcard — so this
        // case is really asserting that the four served names are the four the
        // sealed surface promises.
        assert_eq!(request_name(&KernelRequest::Health {}), "health");
        assert_eq!(request_name(&KernelRequest::Status {}), "status");
        assert_eq!(request_name(&KernelRequest::Watermark {}), "watermark");
        assert_eq!(
            request_name(&KernelRequest::VerifySealed {}),
            "verify_sealed"
        );
    }
}
