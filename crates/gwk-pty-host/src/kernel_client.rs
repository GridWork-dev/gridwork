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

use base64::prelude::{BASE64_STANDARD, Engine as _};
use gwk_domain::frame::{PtyDelta, PtyFrame};
use gwk_domain::ids::{ByteCount, DispatchNodeId, PtyFrameSeq, PtySessionId, RequestId};
use gwk_domain::protocol::{
    CONNECTION_EGRESS_BYTES_PER_WINDOW, CONNECTION_INGRESS_BYTES_PER_WINDOW, ClientControl,
    FRAME_BODY_MAX_BYTES, FRAME_PAYLOAD_MAX_BYTES, FrameKind, KernelRequest, PTY_INPUT_CAPABILITY,
    PTY_INPUT_MAX_BASE64_BYTES, PTY_INPUT_MAX_BYTES, PTY_RAW_CAPABILITY, ProjectionKind,
    ProjectionRecord, ProtocolVersion, ServerControl,
};
use gwk_domain::{CommandEnvelope, KernelErrorCode, KernelResult};
use gwk_kernel::wire::frame::{Budget, Incoming, read_frame, write_frame};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::registry::Attacher;

/// This host's `client` label on the handshake, for the kernel's own logs.
pub const CLIENT_LABEL: &str = "gwk-pty-host";

/// Controls decoded ahead of the publisher loop. Bounded independently of the
/// kernel's queue so a host that stops servicing input cannot grow without end.
const INCOMING_QUEUE_DEPTH: usize = 64;

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
    #[error("could not apply input command {command_id} to {session_id}:{generation}: {message}")]
    Input {
        command_id: gwk_domain::ids::CommandId,
        session_id: PtySessionId,
        generation: gwk_domain::ids::PtySessionGeneration,
        message: String,
    },
}

/// One outbound connection to the kernel, already through the handshake.
#[derive(Debug)]
pub struct KernelClient {
    writer: OwnedWriteHalf,
    egress: Budget,
    incoming: mpsc::Receiver<Result<ServerControl, gwk_kernel::WireError>>,
    reader: JoinHandle<()>,
    /// Requests are numbered rather than randomized, matching `gridwork`'s
    /// own client: this connection asks one question at a time, and a
    /// counter is enough to tell a stray answer from the one this call is
    /// waiting on.
    issued: u64,
    raw_enabled: bool,
    input: Option<Attacher>,
}

impl KernelClient {
    /// Connect to the kernel's Unix socket at `path` and complete the
    /// handshake.
    pub async fn connect(path: &Path) -> Result<Self, KernelClientError> {
        Self::connect_inner(path, None).await
    }

    /// Connect the publisher for one local session. Reverse input controls on
    /// this connection are delivered through the same session handle.
    pub async fn connect_for_session(
        path: &Path,
        input: Attacher,
    ) -> Result<Self, KernelClientError> {
        Self::connect_inner(path, Some(input)).await
    }

    async fn connect_inner(
        path: &Path,
        input: Option<Attacher>,
    ) -> Result<Self, KernelClientError> {
        let stream =
            UnixStream::connect(path)
                .await
                .map_err(|source| KernelClientError::Connect {
                    path: path.display().to_string(),
                    source,
                })?;
        let (reader, writer) = stream.into_split();
        let (incoming_tx, incoming) = mpsc::channel(INCOMING_QUEUE_DEPTH);
        let reader = tokio::spawn(read_controls(reader, incoming_tx));
        let mut client = Self {
            writer,
            egress: Budget::new(
                CONNECTION_INGRESS_BYTES_PER_WINDOW,
                CONNECTION_EGRESS_BYTES_PER_WINDOW,
            ),
            incoming,
            reader,
            issued: 0,
            raw_enabled: false,
            input,
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
                    gwk_domain::CapabilityName::new(PTY_INPUT_CAPABILITY)
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
        write_frame(&mut self.writer, FrameKind::PtyRaw, bytes, &mut self.egress).await?;
        loop {
            let Some(control) = self.receive().await? else {
                return Err(KernelClientError::ClosedDuring("the raw publish"));
            };
            let Some(control) = Self::consume_input(self.input.as_ref(), control).await? else {
                continue;
            };
            match control {
                ServerControl::Response {
                    request_id: answered,
                    result: KernelResult::PtyPublished { .. },
                } if answered == request_id => return Ok(()),
                ServerControl::Response {
                    request_id: answered,
                    result: KernelResult::Error { code, message, .. },
                } if answered == request_id => {
                    return Err(KernelClientError::Refused {
                        refused,
                        code,
                        message,
                    });
                }
                ServerControl::EventBatch { .. }
                | ServerControl::StreamClosed { .. }
                | ServerControl::PtyDeltaBatch { .. }
                | ServerControl::PtyStreamClosed { .. }
                | ServerControl::PtyRawSnapshot { .. }
                | ServerControl::PtyRawChunk { .. }
                | ServerControl::PtyRawResized { .. }
                | ServerControl::PtyRawStreamClosed { .. } => {}
                other => {
                    return Err(KernelClientError::Unexpected {
                        waited_on: "a raw publish acknowledgement",
                        found: format!("{other:?}"),
                    });
                }
            }
        }
    }

    /// Wait for one reverse control while the publisher has no request in
    /// flight. This method only dequeues and returns ownership: applying an
    /// input happens after the surrounding `tokio::select!` chooses this arm,
    /// so cancellation can never discard a command mid-write.
    pub async fn wait_for_control(&mut self) -> Result<ServerControl, KernelClientError> {
        let Some(control) = self.receive().await? else {
            return Err(KernelClientError::ClosedDuring("PTY input wait"));
        };
        Ok(control)
    }

    pub async fn apply_input_control(
        &self,
        control: ServerControl,
    ) -> Result<(), KernelClientError> {
        match Self::consume_input(self.input.as_ref(), control).await? {
            None => Ok(()),
            Some(other) => Err(KernelClientError::Unexpected {
                waited_on: "PTY input",
                found: format!("{other:?}"),
            }),
        }
    }

    /// Consume a reverse input control, returning non-input controls unchanged.
    async fn consume_input(
        input: Option<&Attacher>,
        control: ServerControl,
    ) -> Result<Option<ServerControl>, KernelClientError> {
        let ServerControl::PtyInput {
            command_id,
            session_id,
            generation,
            byte_size,
            data_base64,
        } = control
        else {
            return Ok(Some(control));
        };
        let Some(input) = input else {
            return Err(KernelClientError::Input {
                command_id,
                session_id,
                generation,
                message: "this connection has no local session input route".to_owned(),
            });
        };
        if input.id() != &session_id {
            return Err(KernelClientError::Input {
                command_id,
                session_id,
                generation,
                message: format!("this publisher owns local session {}", input.id()),
            });
        }
        if data_base64.as_str().len() > PTY_INPUT_MAX_BASE64_BYTES {
            return Err(KernelClientError::Input {
                command_id,
                session_id,
                generation,
                message: format!(
                    "base64 carrier is {} bytes, maximum {PTY_INPUT_MAX_BASE64_BYTES}",
                    data_base64.as_str().len()
                ),
            });
        }
        let bytes = BASE64_STANDARD
            .decode(data_base64.as_str())
            .map_err(|error| KernelClientError::Input {
                command_id: command_id.clone(),
                session_id: session_id.clone(),
                generation: generation.clone(),
                message: format!("invalid base64: {error}"),
            })?;
        let actual = u64::try_from(bytes.len()).map_err(|_| KernelClientError::Input {
            command_id: command_id.clone(),
            session_id: session_id.clone(),
            generation: generation.clone(),
            message: "decoded byte count does not fit u64".to_owned(),
        })?;
        if bytes.len() > PTY_INPUT_MAX_BYTES || actual != byte_size.value() {
            return Err(KernelClientError::Input {
                command_id,
                session_id,
                generation,
                message: format!(
                    "header declares {byte_size} bytes, decoded {actual}, maximum {PTY_INPUT_MAX_BYTES}"
                ),
            });
        }
        input
            .input(bytes)
            .await
            .map_err(|error| KernelClientError::Input {
                command_id,
                session_id,
                generation,
                message: error.to_string(),
            })?;
        Ok(None)
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
            let Some(control) = self.receive().await? else {
                return Err(KernelClientError::ClosedDuring("the request"));
            };
            let Some(control) = Self::consume_input(self.input.as_ref(), control).await? else {
                continue;
            };
            match control {
                ServerControl::Response {
                    request_id: answered,
                    result,
                } if answered == request_id => return Ok(result),
                // A batch from a subscription or attach opened earlier on
                // this connection can arrive between a request and its
                // response — this client never opens either, but the wire
                // does not know that, so the loop still has to skip past one.
                ServerControl::EventBatch { .. }
                | ServerControl::StreamClosed { .. }
                | ServerControl::PtyDeltaBatch { .. }
                | ServerControl::PtyStreamClosed { .. } => {}
                other => {
                    return Err(KernelClientError::Unexpected {
                        waited_on: "a response",
                        found: format!("{other:?}"),
                    });
                }
            }
        }
    }

    async fn receive(&mut self) -> Result<Option<ServerControl>, KernelClientError> {
        match self.incoming.recv().await {
            Some(Ok(control)) => Ok(Some(control)),
            Some(Err(error)) => Err(error.into()),
            None => Ok(None),
        }
    }

    async fn send(&mut self, control: &ClientControl) -> Result<(), KernelClientError> {
        let body = serde_json::to_vec(control).map_err(|e| {
            gwk_kernel::WireError::new(KernelErrorCode::Schema, format!("serialize a request: {e}"))
        })?;
        write_frame(&mut self.writer, FrameKind::Json, &body, &mut self.egress).await?;
        Ok(())
    }

    fn next_id(&mut self) -> RequestId {
        self.issued += 1;
        RequestId::new(format!("{CLIENT_LABEL}-{}", self.issued))
    }
}

impl Drop for KernelClient {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

/// Own the socket's read half for its whole lifetime. A frame decode is never
/// cancelled while the connection continues; consumers select only on the
/// cancellation-safe bounded channel this task feeds.
async fn read_controls(
    mut reader: OwnedReadHalf,
    controls: mpsc::Sender<Result<ServerControl, gwk_kernel::WireError>>,
) {
    let mut ingress = Budget::new(
        CONNECTION_INGRESS_BYTES_PER_WINDOW,
        CONNECTION_EGRESS_BYTES_PER_WINDOW,
    );
    loop {
        let control = match read_frame(&mut reader, FRAME_BODY_MAX_BYTES, &mut ingress).await {
            Ok(Incoming::Closed) => return,
            Ok(Incoming::Frame(frame)) if frame.kind == FrameKind::Json => {
                serde_json::from_slice(&frame.body).map_err(|error| {
                    gwk_kernel::WireError::new(KernelErrorCode::Schema, format!("decode: {error}"))
                })
            }
            Ok(Incoming::Frame(frame)) => Err(gwk_kernel::WireError::new(
                KernelErrorCode::Handshake,
                format!("kind {:?} carries no control value", frame.kind),
            )),
            Err(error) => Err(error),
        };
        let failed = control.is_err();
        if controls.send(control).await.is_err() || failed {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::SessionRegistry;
    use crate::session::{RestartPolicy, SessionConfig};
    use gwk_domain::ids::{CommandId, PtySessionGeneration};
    use gwk_domain::protocol::PtyInputData;
    use std::time::Duration;

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

    #[tokio::test]
    async fn reverse_input_control_decodes_and_writes_the_exact_bytes_to_the_child() {
        let id = PtySessionId::new("input-control");
        let mut registry = SessionRegistry::new();
        registry
            .spawn(
                id.clone(),
                Box::new(|cols, rows| {
                    gwk_pty::Session::spawn(pty_process::Command::new("/bin/cat"), cols, rows)
                }),
                SessionConfig {
                    cols: 40,
                    rows: 6,
                    recording_cap: 1024,
                    retained_batches: 1024,
                    restart: RestartPolicy::Never,
                },
            )
            .await
            .expect("spawn cat");
        let input = registry.attacher(&id).expect("attacher");
        let bytes = b"hello\n";
        let consumed = KernelClient::consume_input(
            Some(&input),
            ServerControl::PtyInput {
                command_id: CommandId::new("input-1"),
                session_id: id.clone(),
                generation: PtySessionGeneration::new("life-1"),
                byte_size: ByteCount::new(bytes.len() as u64),
                data_base64: PtyInputData::new(BASE64_STANDARD.encode(bytes)),
            },
        )
        .await
        .expect("consume input");
        assert!(consumed.is_none(), "input controls are consumed in place");

        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(snapshot) = registry.snapshot(&id).await {
                    let text = snapshot.frame.cells().expect("snapshot expands")[0]
                        .iter()
                        .map(|cell| cell.glyph.as_str())
                        .collect::<String>();
                    if text.starts_with("hello") {
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("cat echoed the input");

        let invalid = KernelClient::consume_input(
            Some(&input),
            ServerControl::PtyInput {
                command_id: CommandId::new("input-2"),
                session_id: id.clone(),
                generation: PtySessionGeneration::new("life-1"),
                byte_size: ByteCount::new(99),
                data_base64: PtyInputData::new(BASE64_STANDARD.encode(b"x")),
            },
        )
        .await
        .expect_err("mismatched byte count");
        assert!(matches!(invalid, KernelClientError::Input { .. }));

        registry.stop(&id).await.expect("stop cat");
    }
}
