//! One connection, from hello to hangup — and the accept loop that owns them.
//!
//! What is served here is the SEALED surface: hello, health, status, watermark,
//! and the fresh-epoch proof. Those are exactly the requests a kernel can answer
//! before it has been activated, so a sealed daemon is COMPLETE at this layer
//! rather than half-built — an operator can bring one up, watch it, and prove
//! its epoch without any of the request types task 19 adds.
//!
//! The remaining requests are matched BY NAME and refused, not swept up by a
//! wildcard: the compiler has to notice when the protocol grows a request, and
//! a reader has to be able to see which ones this layer does not answer yet.

use std::sync::Arc;

use gwk_domain::ids::{EventCount, EventId, Seq, WriterEpoch};
use gwk_domain::port::EventStore;
use gwk_domain::protocol::{
    CONNECTION_EGRESS_MAX_BYTES, CONNECTION_INGRESS_MAX_BYTES, CONTRACT_VERSION,
    FRAME_BODY_MAX_BYTES, FrameKind, KernelErrorCode, KernelRequest, KernelResult, ServerControl,
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
    pub fn new(
        store: PgEventStore,
        public_revision: String,
        writer_epoch: WriterEpoch,
    ) -> Result<Self> {
        if !is_public_revision(&public_revision) {
            return Err(KernelError::Config(format!(
                "public revision {public_revision:?} is not a full 40-hex revision"
            )));
        }
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

            // Named one by one so the protocol cannot grow a request this
            // layer silently drops. Task 19 fills these in; until it does the
            // refusal says which request it was, not "unsupported".
            KernelRequest::SubmitCommand { .. }
            | KernelRequest::GetProjection { .. }
            | KernelRequest::ListProjection { .. }
            | KernelRequest::ReadEvents { .. }
            | KernelRequest::SubscribeEvents { .. }
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
