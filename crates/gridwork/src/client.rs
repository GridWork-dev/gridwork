//! The client half of the local wire.
//!
//! Deliberately thin. The codec, the bounds, and the strict decoder are the
//! kernel crate's and are used as-is rather than reimplemented here — a second
//! implementation of the framing is a second thing to keep in step, and the
//! first divergence would be a client that cannot talk to its own daemon.
//!
//! There is no authentication step. The socket's mode and the daemon's
//! same-EUID peer check are the boundary (ADR 0002), so a hello carries a label
//! and a capability wish list and nothing that could widen what this connection
//! may do.

use std::path::Path;

use gwk_domain::ids::{RequestId, Seq};
use gwk_domain::protocol::{
    CONNECTION_EGRESS_BYTES_PER_WINDOW, CONNECTION_INGRESS_BYTES_PER_WINDOW, ClientControl,
    FRAME_BODY_MAX_BYTES, FrameKind, KernelErrorCode, KernelRequest, KernelResult, ProtocolVersion,
    ServerControl,
};
use gwk_kernel::wire::frame::{Budget, Incoming, read_frame, write_frame};
use tokio::net::UnixStream;

use crate::exit::Failure;

/// One connection, already through the handshake.
pub struct Client {
    stream: UnixStream,
    budget: Budget,
    /// Requests are numbered rather than randomized: one connection asks one
    /// question at a time here, and a counter makes a transcript readable.
    issued: u64,
}

impl Client {
    /// Connect and complete the handshake, or say why not.
    pub async fn connect(path: &Path) -> Result<(Self, ServerControl), Failure> {
        let stream = UnixStream::connect(path)
            .await
            .map_err(|e| Failure::unreachable(format!("connect {}: {e}", path.display())))?;
        let mut client = Self {
            stream,
            budget: Budget::new(
                CONNECTION_INGRESS_BYTES_PER_WINDOW,
                CONNECTION_EGRESS_BYTES_PER_WINDOW,
            ),
            issued: 0,
        };
        client
            .send(&ClientControl::Hello {
                protocol_major: ProtocolVersion::V1,
                protocol_minor: 0,
                // Nothing asked for. A capability is a feature list, and this
                // client uses only what v1 requires of every daemon.
                capabilities: Vec::new(),
                client: Some("gw".to_owned()),
            })
            .await?;
        let ack = client
            .receive()
            .await?
            .ok_or_else(|| Failure::unreachable("the daemon closed before acknowledging"))?;
        match &ack {
            ServerControl::HelloAck { .. } => Ok((client, ack)),
            // A refusal at the handshake is the daemon's answer, reported as it
            // arrived rather than translated.
            ServerControl::HelloRefusal { code, message } => {
                Err(Failure::new(*code, message.clone()))
            }
            other => Err(Failure::unreachable(format!(
                "the daemon answered a hello with {other:?}"
            ))),
        }
    }

    /// Ask one question and take its answer.
    pub async fn ask(&mut self, request: KernelRequest) -> Result<KernelResult, Failure> {
        let request_id = self.next_id();
        self.send(&ClientControl::Request {
            request_id: request_id.clone(),
            request,
        })
        .await?;
        loop {
            let frame = self
                .receive()
                .await?
                .ok_or_else(|| Failure::unreachable("the daemon closed without answering"))?;
            match frame {
                ServerControl::Response {
                    request_id: answered,
                    result,
                } if answered == request_id => return Ok(result),
                // A batch from a subscription opened earlier on this connection
                // can arrive between a request and its response. Skipped rather
                // than mistaken for the answer — that is what the id is for.
                ServerControl::EventBatch { .. } | ServerControl::StreamClosed { .. } => {}
                other => {
                    return Err(Failure::unreachable(format!(
                        "the daemon answered {request_id} with {other:?}"
                    )));
                }
            }
        }
    }

    /// Open a subscription and return the cursor it starts from.
    ///
    /// The request id comes back because every batch carries it, and a caller
    /// following one stream still has to check.
    pub async fn subscribe(&mut self, cursor: Option<Seq>) -> Result<RequestId, Failure> {
        let request_id = self.next_id();
        self.send(&ClientControl::Request {
            request_id: request_id.clone(),
            request: KernelRequest::SubscribeEvents { cursor },
        })
        .await?;
        match self
            .receive()
            .await?
            .ok_or_else(|| Failure::unreachable("the daemon closed without acknowledging"))?
        {
            ServerControl::Response { result, .. } => match result {
                KernelResult::Subscribed { .. } => Ok(request_id),
                KernelResult::Error { code, message, .. } => Err(Failure::new(code, message)),
                other => Err(Failure::unreachable(format!(
                    "subscribe answered with {other:?}"
                ))),
            },
            other => Err(Failure::unreachable(format!(
                "subscribe answered with {other:?}"
            ))),
        }
    }

    /// The next thing the daemon sends, or `None` when it hangs up.
    pub async fn receive(&mut self) -> Result<Option<ServerControl>, Failure> {
        let frame = read_frame(&mut self.stream, FRAME_BODY_MAX_BYTES, &mut self.budget)
            .await
            .map_err(|e| Failure::new(e.code, e.message))?;
        match frame {
            Incoming::Closed => Ok(None),
            Incoming::Frame(frame) => {
                if frame.kind != FrameKind::Json {
                    return Err(Failure::new(
                        KernelErrorCode::Handshake,
                        format!("kind {:?} carries no control value", frame.kind),
                    ));
                }
                serde_json::from_slice(&frame.body)
                    .map(Some)
                    .map_err(|e| Failure::new(KernelErrorCode::Schema, format!("decode: {e}")))
            }
        }
    }

    async fn send(&mut self, control: &ClientControl) -> Result<(), Failure> {
        let body = serde_json::to_vec(control)
            .map_err(|e| Failure::internal(format!("serialize a request: {e}")))?;
        write_frame(&mut self.stream, FrameKind::Json, &body, &mut self.budget)
            .await
            .map_err(|e| Failure::new(e.code, e.message))
    }

    fn next_id(&mut self) -> RequestId {
        self.issued += 1;
        RequestId::new(format!("gw-{}", self.issued))
    }
}
