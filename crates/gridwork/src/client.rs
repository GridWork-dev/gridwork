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

use gwk_domain::ids::{PtyFrameSeq, PtySessionGeneration, PtySessionId, RequestId, Seq};
use gwk_domain::protocol::{
    CONNECTION_EGRESS_BYTES_PER_WINDOW, CONNECTION_INGRESS_BYTES_PER_WINDOW, ClientControl,
    FRAME_BODY_MAX_BYTES, FRAME_PAYLOAD_MAX_BYTES, FrameKind, KernelErrorCode, KernelRequest,
    KernelResult, PTY_RAW_CAPABILITY, ProtocolVersion, ServerControl,
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
    raw_enabled: bool,
}

/// One item from a raw PTY attach. Snapshot/output variants carry arbitrary
/// bytes; no UTF-8 conversion occurs on this edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtyRawMessage {
    Snapshot {
        request_id: RequestId,
        generation: PtySessionGeneration,
        seq: Option<PtyFrameSeq>,
        rows: u16,
        cols: u16,
        bytes: Vec<u8>,
    },
    Output {
        request_id: RequestId,
        generation: PtySessionGeneration,
        seq: PtyFrameSeq,
        bytes: Vec<u8>,
    },
    Resized {
        request_id: RequestId,
        generation: PtySessionGeneration,
        seq: PtyFrameSeq,
        rows: u16,
        cols: u16,
    },
    Closed {
        request_id: RequestId,
        generation: PtySessionGeneration,
        code: KernelErrorCode,
        last_seq: Option<PtyFrameSeq>,
    },
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
            raw_enabled: false,
        };
        client
            .send(&ClientControl::Hello {
                protocol_major: ProtocolVersion::V1,
                protocol_minor: 0,
                // Nothing asked for. A capability is a feature list, and this
                // client uses only what v1 requires of every daemon.
                capabilities: vec![
                    gwk_domain::CapabilityName::new(PTY_RAW_CAPABILITY)
                        .expect("the protocol's own capability name is valid"),
                ],
                client: Some("gw".to_owned()),
            })
            .await?;
        let ack = client
            .receive()
            .await?
            .ok_or_else(|| Failure::unreachable("the daemon closed before acknowledging"))?;
        match &ack {
            ServerControl::HelloAck { capabilities, .. } => {
                client.raw_enabled = capabilities
                    .iter()
                    .any(|capability| capability.as_str() == PTY_RAW_CAPABILITY);
                Ok((client, ack))
            }
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

    /// Open one hosted-session attach and return its typed acknowledgement.
    /// The caller retains the request id because every later delta carries it.
    pub async fn attach_pty(
        &mut self,
        session_id: PtySessionId,
        generation: Option<PtySessionGeneration>,
        cursor: Option<PtyFrameSeq>,
    ) -> Result<(RequestId, ServerControl), Failure> {
        let request_id = self.next_id();
        self.send(&ClientControl::Request {
            request_id: request_id.clone(),
            request: KernelRequest::PtyAttach {
                session_id,
                generation,
                cursor,
            },
        })
        .await?;
        let response = self
            .receive()
            .await?
            .ok_or_else(|| Failure::unreachable("the daemon closed without acknowledging"))?;
        match &response {
            ServerControl::Response {
                request_id: answered,
                result: KernelResult::PtyAttached { .. },
            } if answered == &request_id => Ok((request_id, response)),
            ServerControl::Response {
                request_id: answered,
                result: KernelResult::Error { code, message, .. },
            } if answered == &request_id => Err(Failure::new(*code, message.clone())),
            other => Err(Failure::unreachable(format!(
                "attach answered {request_id} with {other:?}"
            ))),
        }
    }

    /// Open the raw fallback for one hosted session. The capability check keeps
    /// a new client from sending an unknown request to an older daemon.
    pub async fn attach_pty_raw(
        &mut self,
        session_id: PtySessionId,
        generation: Option<PtySessionGeneration>,
        cursor: Option<PtyFrameSeq>,
    ) -> Result<(RequestId, ServerControl), Failure> {
        if !self.raw_enabled {
            return Err(Failure::new(
                KernelErrorCode::Capability,
                format!("{PTY_RAW_CAPABILITY} was not negotiated"),
            ));
        }
        let request_id = self.next_id();
        self.send(&ClientControl::Request {
            request_id: request_id.clone(),
            request: KernelRequest::PtyRawAttach {
                session_id,
                generation,
                cursor,
            },
        })
        .await?;
        let response = self
            .receive()
            .await?
            .ok_or_else(|| Failure::unreachable("the daemon closed without acknowledging"))?;
        match &response {
            ServerControl::Response {
                request_id: answered,
                result: KernelResult::PtyRawAttached { .. },
            } if answered == &request_id => Ok((request_id, response)),
            ServerControl::Response {
                request_id: answered,
                result: KernelResult::Error { code, message, .. },
            } if answered == &request_id => Err(Failure::new(*code, message.clone())),
            other => Err(Failure::unreachable(format!(
                "raw attach answered {request_id} with {other:?}"
            ))),
        }
    }

    /// Receive one logical raw delivery. Snapshot/output headers consume the
    /// immediately following kind `0x02` frame before returning.
    pub async fn receive_pty_raw(&mut self) -> Result<Option<PtyRawMessage>, Failure> {
        let frame = read_frame(&mut self.stream, FRAME_BODY_MAX_BYTES, &mut self.budget)
            .await
            .map_err(|e| Failure::new(e.code, e.message))?;
        let Incoming::Frame(frame) = frame else {
            return Ok(None);
        };
        if frame.kind != FrameKind::Json {
            return Err(Failure::new(
                KernelErrorCode::Handshake,
                "a raw PTY payload arrived without its JSON header",
            ));
        }
        let control: ServerControl = serde_json::from_slice(&frame.body)
            .map_err(|e| Failure::new(KernelErrorCode::Schema, format!("decode: {e}")))?;
        match control {
            ServerControl::PtyRawSnapshot {
                request_id,
                generation,
                seq,
                rows,
                cols,
                byte_size,
            } => Ok(Some(PtyRawMessage::Snapshot {
                request_id,
                generation,
                seq,
                rows,
                cols,
                bytes: self.receive_raw_payload(byte_size.value()).await?,
            })),
            ServerControl::PtyRawChunk {
                request_id,
                generation,
                seq,
                byte_size,
            } => Ok(Some(PtyRawMessage::Output {
                request_id,
                generation,
                seq,
                bytes: self.receive_raw_payload(byte_size.value()).await?,
            })),
            ServerControl::PtyRawResized {
                request_id,
                generation,
                seq,
                rows,
                cols,
            } => Ok(Some(PtyRawMessage::Resized {
                request_id,
                generation,
                seq,
                rows,
                cols,
            })),
            ServerControl::PtyRawStreamClosed {
                request_id,
                generation,
                code,
                last_seq,
            } => Ok(Some(PtyRawMessage::Closed {
                request_id,
                generation,
                code,
                last_seq,
            })),
            other => Err(Failure::unreachable(format!(
                "raw attach received {other:?}"
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

    async fn receive_raw_payload(&mut self, byte_size: u64) -> Result<Vec<u8>, Failure> {
        if byte_size > FRAME_PAYLOAD_MAX_BYTES as u64 {
            return Err(Failure::new(
                KernelErrorCode::FrameSize,
                format!(
                    "raw PTY byte_size {byte_size} exceeds the {FRAME_PAYLOAD_MAX_BYTES}-byte payload maximum"
                ),
            ));
        }
        let expected = usize::try_from(byte_size).map_err(|_| {
            Failure::new(
                KernelErrorCode::FrameSize,
                "raw PTY byte_size does not fit this client",
            )
        })?;
        let frame = read_frame(&mut self.stream, FRAME_BODY_MAX_BYTES, &mut self.budget)
            .await
            .map_err(|e| Failure::new(e.code, e.message))?;
        let Incoming::Frame(frame) = frame else {
            return Err(Failure::unreachable(
                "the daemon closed before the announced raw PTY payload",
            ));
        };
        if frame.kind != FrameKind::PtyRaw || frame.body.len() != expected {
            return Err(Failure::new(
                KernelErrorCode::FrameSize,
                format!(
                    "raw PTY header announced {expected} bytes, got kind {:?} with {}",
                    frame.kind,
                    frame.body.len()
                ),
            ));
        }
        Ok(frame.body)
    }

    fn next_id(&mut self) -> RequestId {
        self.issued += 1;
        RequestId::new(format!("gw-{}", self.issued))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gwk_domain::ByteCount;

    #[tokio::test]
    async fn raw_receive_pairs_the_header_with_byte_exact_payload() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("socket pair");
        let bytes = vec![0x00, 0xff, 0x1b, b'[', 0xc3];
        let sent = bytes.clone();
        let server = tokio::spawn(async move {
            let header = ServerControl::PtyRawChunk {
                request_id: RequestId::new("raw-1"),
                generation: PtySessionGeneration::new("life-1"),
                seq: PtyFrameSeq::new(7),
                byte_size: ByteCount::new(sent.len() as u64),
            };
            let body = serde_json::to_vec(&header).expect("serialize header");
            let mut budget = Budget::new(1 << 20, 1 << 20);
            write_frame(&mut server_stream, FrameKind::Json, &body, &mut budget)
                .await
                .expect("write header");
            write_frame(&mut server_stream, FrameKind::PtyRaw, &sent, &mut budget)
                .await
                .expect("write raw bytes");
        });
        let mut client = Client {
            stream: client_stream,
            budget: Budget::new(1 << 20, 1 << 20),
            issued: 0,
            raw_enabled: true,
        };

        match client
            .receive_pty_raw()
            .await
            .expect("receive")
            .expect("one message")
        {
            PtyRawMessage::Output {
                seq, bytes: got, ..
            } => {
                assert_eq!(seq, PtyFrameSeq::new(7));
                assert_eq!(got, bytes);
            }
            other => panic!("{other:?}"),
        }
        server.await.expect("join server");
    }

    #[tokio::test]
    async fn raw_receive_refuses_an_impossible_header_without_waiting_for_payload() {
        let (client_stream, mut server_stream) = UnixStream::pair().expect("socket pair");
        let header = ServerControl::PtyRawChunk {
            request_id: RequestId::new("raw-too-large"),
            generation: PtySessionGeneration::new("life-1"),
            seq: PtyFrameSeq::new(8),
            // The frame ceiling includes its kind byte, so this declared
            // payload can never fit in one legal frame.
            byte_size: ByteCount::new(FRAME_BODY_MAX_BYTES as u64),
        };
        let body = serde_json::to_vec(&header).expect("serialize header");
        let mut server_budget = Budget::new(1 << 20, 1 << 20);
        write_frame(
            &mut server_stream,
            FrameKind::Json,
            &body,
            &mut server_budget,
        )
        .await
        .expect("write header");

        let mut client = Client {
            stream: client_stream,
            budget: Budget::new(1 << 20, 1 << 20),
            issued: 0,
            raw_enabled: true,
        };
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            client.receive_pty_raw(),
        )
        .await
        .expect("the impossible size is refused from the header alone")
        .expect_err("an impossible raw payload size must be refused");
        assert_eq!(result.code, KernelErrorCode::FrameSize);

        // Keep the writer alive through the assertion: success cannot come from
        // observing EOF while waiting for the absent payload.
        drop(server_stream);
    }
}
