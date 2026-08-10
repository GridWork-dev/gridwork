//! The thin, outbound-only client half of the kernel wire.
//!
//! This mirrors `gridwork`'s own `Client` (`crates/gridwork/src/client.rs`)
//! rather than depending on it: `gridwork` is the `gw` CLI's binary crate,
//! and a resident daemon depending on a CLI binary's library surface for
//! its own kernel traffic would be the wrong direction for that dependency
//! to point. Both clients use the SAME codec and control types from
//! `gwk-kernel`/`gwk-domain` — the framing is never reimplemented, only the
//! thin request/response loop around it.
//!
//! Outbound only, by construction: this type owns a [`UnixStream`] it
//! connects, never one it accepts, and nothing here binds a socket. That is
//! this crate's own hard floor, not a convention borrowed for this file —
//! see the crate root doc.
//!
//! Derivation: none — original outbound glue over this repository's domain
//! controls and already-covered kernel codec (`gwk_kernel::wire::frame`);
//! it does not reproduce an external framing or terminal protocol.

use std::path::Path;

use gwk_domain::frame::{PtyDelta, PtyFrame};
use gwk_domain::ids::{ByteCount, DispatchNodeId, PtyFrameSeq, PtySessionId, RequestId};
use gwk_domain::protocol::{
    CONNECTION_EGRESS_BYTES_PER_WINDOW, CONNECTION_INGRESS_BYTES_PER_WINDOW, ClientControl,
    FRAME_BODY_MAX_BYTES, FRAME_PAYLOAD_MAX_BYTES, FrameKind, KernelRequest, PTY_RAW_CAPABILITY,
    ProjectionKind, ProjectionRecord, ProtocolVersion, ServerControl,
};
use gwk_domain::{CommandEnvelope, KernelErrorCode, KernelResult};
use gwk_kernel::wire::frame::{Budget, Incoming, read_frame, write_frame};
use tokio::net::UnixStream;

/// This host's `client` label on the handshake, for the kernel's own logs.
pub const CLIENT_LABEL: &str = "gwk-pty-host";

/// Why a kernel connection or request could not be completed.
#[derive(Debug, thiserror::Error)]
pub enum KernelClientError {
    #[error("connect {path}: {source}")]
    Connect {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("the kernel wire refused: {0}")]
    Wire(#[from] gwk_kernel::WireError),
    #[error("the daemon closed the connection during {0}")]
    ClosedDuring(&'static str),
    #[error("the kernel refused the hello: {code:?} {message}")]
    HelloRefused {
        code: KernelErrorCode,
        message: String,
    },
    #[error("the daemon answered {waited_on} with an unexpected frame: {found}")]
    Unexpected {
        waited_on: &'static str,
        found: String,
    },
    /// A typed refusal on the PTY publish surface. Surfaced as an error
    /// rather than classified like [`crate::origination::Outcome`] because
    /// every refusal here means the same thing to the one caller: this
    /// connection's view of the session is stale — drop it and republish
    /// from a fresh local attach.
    #[error("the kernel refused {refused}: {code:?} {message}")]
    Refused {
        refused: &'static str,
        code: KernelErrorCode,
        message: String,
    },
}

/// One outbound connection to the kernel, already through the handshake.
#[derive(Debug)]
pub struct KernelClient {
    stream: UnixStream,
    budget: Budget,
    /// Requests are numbered rather than randomized, matching `gridwork`'s
    /// own client: this connection asks one question at a time, and a
    /// counter is enough to tell a stray answer from the one this call is
    /// waiting on.
    issued: u64,
    raw_enabled: bool,
}

impl KernelClient {
    /// Connect to the kernel's Unix socket at `path` and complete the
    /// handshake.
    pub async fn connect(path: &Path) -> Result<Self, KernelClientError> {
        let stream =
            UnixStream::connect(path)
                .await
                .map_err(|source| KernelClientError::Connect {
                    path: path.display().to_string(),
                    source,
                })?;
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
                // Nothing asked for: this client uses only what v1 requires
                // of every daemon, the same posture `gridwork`'s own client
                // takes.
                capabilities: vec![
                    gwk_domain::CapabilityName::new(PTY_RAW_CAPABILITY)
                        .expect("the protocol's own capability name is valid"),
                ],
                client: Some(CLIENT_LABEL.to_owned()),
            })
            .await?;
        match client.receive().await? {
            Some(ServerControl::HelloAck { capabilities, .. }) => {
                client.raw_enabled = capabilities
                    .iter()
                    .any(|capability| capability.as_str() == PTY_RAW_CAPABILITY);
                Ok(client)
            }
            Some(ServerControl::HelloRefusal { code, message }) => {
                Err(KernelClientError::HelloRefused { code, message })
            }
            Some(other) => Err(KernelClientError::Unexpected {
                waited_on: "the hello",
                found: format!("{other:?}"),
            }),
            None => Err(KernelClientError::ClosedDuring("the hello")),
        }
    }

    /// Submit one command envelope and take the kernel's answer as a VALUE.
    /// A refusal is `KernelResult::Error`, not a second error channel —
    /// [`crate::origination::submit`] is what classifies it.
    pub async fn submit(
        &mut self,
        envelope: CommandEnvelope,
    ) -> Result<KernelResult, KernelClientError> {
        self.ask(KernelRequest::SubmitCommand { envelope }).await
    }

    /// A dispatch node's current version, or `None` when the kernel holds no
    /// such node. This is the re-read a stale `TransitionDispatchNode` needs
    /// before it may be re-originated — [`crate::origination::Outcome`]'s own
    /// doc forbids guessing the next version.
    pub async fn dispatch_node_version(
        &mut self,
        id: &DispatchNodeId,
    ) -> Result<Option<u32>, KernelClientError> {
        let request = KernelRequest::GetProjection {
            projection: ProjectionKind::DispatchNode,
            id: id.as_str().to_owned(),
        };
        match self.ask(request).await? {
            KernelResult::Projection {
                record: ProjectionRecord::DispatchNode { dispatch_node },
            } => Ok(Some(dispatch_node.version)),
            // Absent is an ANSWER on this projection, same as everywhere
            // else in the contract — not a client-side error.
            KernelResult::Error {
                code: KernelErrorCode::NotFound,
                ..
            } => Ok(None),
            other => Err(KernelClientError::Unexpected {
                waited_on: "a dispatch-node projection",
                found: format!("{other:?}"),
            }),
        }
    }

    /// Publish a session's full screen — the claim on first publish, the
    /// reseed on a reconnect. `seq` is absent only while the session has
    /// produced no frame yet.
    pub async fn publish_snapshot(
        &mut self,
        id: &PtySessionId,
        seq: Option<u64>,
        frame: PtyFrame,
    ) -> Result<(), KernelClientError> {
        let request = KernelRequest::PtyPublishSnapshot {
            session_id: id.clone(),
            seq: seq.map(PtyFrameSeq::new),
            frame,
        };
        match self.ask(request).await? {
            KernelResult::PtyPublished { .. } => Ok(()),
            KernelResult::Error { code, message, .. } => Err(KernelClientError::Refused {
                refused: "a snapshot publish",
                code,
                message,
            }),
            other => Err(KernelClientError::Unexpected {
                waited_on: "a publish acknowledgement",
                found: format!("{other:?}"),
            }),
        }
    }

    /// Publish the delta batch that moves a session's screen to `seq`.
    pub async fn publish_deltas(
        &mut self,
        id: &PtySessionId,
        seq: u64,
        deltas: Vec<PtyDelta>,
    ) -> Result<(), KernelClientError> {
        let request = KernelRequest::PtyPublishDeltas {
            session_id: id.clone(),
            seq: PtyFrameSeq::new(seq),
            deltas,
        };
        match self.ask(request).await? {
            KernelResult::PtyPublished { .. } => Ok(()),
            KernelResult::Error { code, message, .. } => Err(KernelClientError::Refused {
                refused: "a delta publish",
                code,
                message,
            }),
            other => Err(KernelClientError::Unexpected {
                waited_on: "a publish acknowledgement",
                found: format!("{other:?}"),
            }),
        }
    }

    pub fn raw_enabled(&self) -> bool {
        self.raw_enabled
    }

    /// Publish the raw fallback's model-produced VT seed. The JSON header and
    /// kind `0x02` payload are one request; the acknowledgement arrives only
    /// after both have landed.
    pub async fn publish_raw_snapshot(
        &mut self,
        id: &PtySessionId,
        snapshot: &crate::session::RawSnapshot,
    ) -> Result<(), KernelClientError> {
        let request_id = self.next_id();
        self.publish_raw(
            ClientControl::PtyRawPublishSnapshot {
                request_id: request_id.clone(),
                session_id: id.clone(),
                seq: snapshot.seq.map(PtyFrameSeq::new),
                rows: snapshot.rows,
                cols: snapshot.cols,
                byte_size: ByteCount::new(snapshot.bytes.len() as u64),
            },
            request_id,
            &snapshot.bytes,
            "a raw snapshot publish",
        )
        .await
    }

    /// Publish one child-output chunk with no UTF-8 conversion or re-encoding.
    pub async fn publish_raw_output(
        &mut self,
        id: &PtySessionId,
        seq: u64,
        bytes: &[u8],
    ) -> Result<(), KernelClientError> {
        let request_id = self.next_id();
        self.publish_raw(
            ClientControl::PtyRawPublishOutput {
                request_id: request_id.clone(),
                session_id: id.clone(),
                seq: PtyFrameSeq::new(seq),
                byte_size: ByteCount::new(bytes.len() as u64),
            },
            request_id,
            bytes,
            "a raw output publish",
        )
        .await
    }

    /// Publish one resize on the raw fallback's sequence axis.
    pub async fn publish_raw_resize(
        &mut self,
        id: &PtySessionId,
        seq: u64,
        rows: u16,
        cols: u16,
    ) -> Result<(), KernelClientError> {
        match self
            .ask(KernelRequest::PtyPublishRawResize {
                session_id: id.clone(),
                seq: PtyFrameSeq::new(seq),
                rows,
                cols,
            })
            .await?
        {
            KernelResult::PtyPublished { .. } => Ok(()),
            KernelResult::Error { code, message, .. } => Err(KernelClientError::Refused {
                refused: "a raw resize publish",
                code,
                message,
            }),
            other => Err(KernelClientError::Unexpected {
                waited_on: "a raw resize acknowledgement",
                found: format!("{other:?}"),
            }),
        }
    }

    /// Retire a session this connection claimed — the explicit form of what
    /// hanging up does implicitly.
    pub async fn retire(&mut self, id: &PtySessionId) -> Result<(), KernelClientError> {
        let request = KernelRequest::PtyRetire {
            session_id: id.clone(),
        };
        match self.ask(request).await? {
            KernelResult::PtyRetired { .. } => Ok(()),
            KernelResult::Error { code, message, .. } => Err(KernelClientError::Refused {
                refused: "a retire",
                code,
                message,
            }),
            other => Err(KernelClientError::Unexpected {
                waited_on: "a retire acknowledgement",
                found: format!("{other:?}"),
            }),
        }
    }

    async fn publish_raw(
        &mut self,
        header: ClientControl,
        request_id: RequestId,
        bytes: &[u8],
        refused: &'static str,
    ) -> Result<(), KernelClientError> {
        if !self.raw_enabled {
            return Err(KernelClientError::Refused {
                refused,
                code: KernelErrorCode::Capability,
                message: format!("{PTY_RAW_CAPABILITY} was not negotiated"),
            });
        }
        if bytes.len() > FRAME_PAYLOAD_MAX_BYTES {
            return Err(gwk_kernel::WireError::new(
                KernelErrorCode::FrameSize,
                format!(
                    "raw PTY payload is {} bytes; the frame maximum is {FRAME_PAYLOAD_MAX_BYTES}",
                    bytes.len()
                ),
            )
            .into());
        }
        self.send(&header).await?;
        write_frame(&mut self.stream, FrameKind::PtyRaw, bytes, &mut self.budget).await?;
        loop {
            match self.receive().await? {
                Some(ServerControl::Response {
                    request_id: answered,
                    result: KernelResult::PtyPublished { .. },
                }) if answered == request_id => return Ok(()),
                Some(ServerControl::Response {
                    request_id: answered,
                    result: KernelResult::Error { code, message, .. },
                }) if answered == request_id => {
                    return Err(KernelClientError::Refused {
                        refused,
                        code,
                        message,
                    });
                }
                Some(
                    ServerControl::EventBatch { .. }
                    | ServerControl::StreamClosed { .. }
                    | ServerControl::PtyDeltaBatch { .. }
                    | ServerControl::PtyStreamClosed { .. }
                    | ServerControl::PtyRawSnapshot { .. }
                    | ServerControl::PtyRawChunk { .. }
                    | ServerControl::PtyRawResized { .. }
                    | ServerControl::PtyRawStreamClosed { .. },
                ) => {}
                Some(other) => {
                    return Err(KernelClientError::Unexpected {
                        waited_on: "a raw publish acknowledgement",
                        found: format!("{other:?}"),
                    });
                }
                None => return Err(KernelClientError::ClosedDuring("the raw publish")),
            }
        }
    }

    /// Liveness only, for a boot-time connectivity check.
    pub async fn healthy(&mut self) -> Result<bool, KernelClientError> {
        match self.ask(KernelRequest::Health {}).await? {
            KernelResult::Health { ready, .. } => Ok(ready),
            other => Err(KernelClientError::Unexpected {
                waited_on: "health",
                found: format!("{other:?}"),
            }),
        }
    }

    async fn ask(&mut self, request: KernelRequest) -> Result<KernelResult, KernelClientError> {
        let request_id = self.next_id();
        self.send(&ClientControl::Request {
            request_id: request_id.clone(),
            request,
        })
        .await?;
        loop {
            match self.receive().await? {
                Some(ServerControl::Response {
                    request_id: answered,
                    result,
                }) if answered == request_id => return Ok(result),
                // A batch from a subscription or attach opened earlier on
                // this connection can arrive between a request and its
                // response — this client never opens either, but the wire
                // does not know that, so the loop still has to skip past one.
                Some(
                    ServerControl::EventBatch { .. }
                    | ServerControl::StreamClosed { .. }
                    | ServerControl::PtyDeltaBatch { .. }
                    | ServerControl::PtyStreamClosed { .. },
                ) => {}
                Some(other) => {
                    return Err(KernelClientError::Unexpected {
                        waited_on: "a response",
                        found: format!("{other:?}"),
                    });
                }
                None => return Err(KernelClientError::ClosedDuring("the request")),
            }
        }
    }

    async fn receive(&mut self) -> Result<Option<ServerControl>, KernelClientError> {
        let frame = read_frame(&mut self.stream, FRAME_BODY_MAX_BYTES, &mut self.budget).await?;
        match frame {
            Incoming::Closed => Ok(None),
            Incoming::Frame(frame) => {
                if frame.kind != FrameKind::Json {
                    return Err(gwk_kernel::WireError::new(
                        KernelErrorCode::Handshake,
                        format!("kind {:?} carries no control value", frame.kind),
                    )
                    .into());
                }
                serde_json::from_slice(&frame.body).map(Some).map_err(|e| {
                    gwk_kernel::WireError::new(KernelErrorCode::Schema, format!("decode: {e}"))
                        .into()
                })
            }
        }
    }

    async fn send(&mut self, control: &ClientControl) -> Result<(), KernelClientError> {
        let body = serde_json::to_vec(control).map_err(|e| {
            gwk_kernel::WireError::new(KernelErrorCode::Schema, format!("serialize a request: {e}"))
        })?;
        write_frame(&mut self.stream, FrameKind::Json, &body, &mut self.budget).await?;
        Ok(())
    }

    fn next_id(&mut self) -> RequestId {
        self.issued += 1;
        RequestId::new(format!("{CLIENT_LABEL}-{}", self.issued))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn connecting_to_an_absent_socket_is_a_typed_error_not_a_panic() {
        let path = std::env::temp_dir().join(format!(
            "gwk-pty-host-test-absent-{}.sock",
            std::process::id()
        ));
        let error = KernelClient::connect(&path)
            .await
            .expect_err("no daemon is listening here");
        assert!(matches!(error, KernelClientError::Connect { .. }));
    }

    #[tokio::test]
    async fn connecting_to_a_path_that_is_not_a_socket_is_a_typed_error() {
        let dir =
            std::env::temp_dir().join(format!("gwk-pty-host-test-notsock-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("not-a-socket");
        std::fs::write(&path, b"plain file").expect("write a plain file");

        let error = KernelClient::connect(&path)
            .await
            .expect_err("a regular file is not a socket to connect to");
        assert!(matches!(error, KernelClientError::Connect { .. }));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
