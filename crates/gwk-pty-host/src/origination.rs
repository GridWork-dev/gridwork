//! Tying origination together: mint an envelope for a command this host
//! built, submit it, and classify what the kernel answered into something a
//! caller can act on without re-deriving `KernelErrorCode` semantics at
//! every call site — plus [`originate_record`], the whole path for one
//! normalized transcript record: originate its commands, derive each
//! family's idempotency key, submit in the pinned order, and handle each
//! family's results (a stale transition re-reads the node's version once;
//! a refusal stops the record before a later command can contradict the
//! ordering contract).
//!
//! Derivation: none — result classification and submission plumbing only;
//! no terminal byte or process behavior is asserted here.

use gwk_domain::{
    DispatchNodeId, EventEnvelope, IdempotencyKey, KernelCommand, KernelErrorCode, KernelResult,
    ProjectId, Seq, Timestamp,
};

use crate::envelope;
use crate::ingest::{self, TranscriptOriginationError, TranscriptRecord};
use crate::kernel_client::{KernelClient, KernelClientError};

/// What submitting one command settled, once the wire-level [`KernelResult`]
/// is reduced to what a caller of this host actually needs to decide.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Applied — freshly, or as an idempotent replay of an identical prior
    /// submission. The kernel does not distinguish the two on the wire (its
    /// own doc on `KernelResult::CommandApplied`: "an idempotent replay
    /// answers with the original [events]"), and neither does this: both
    /// mean the command landed.
    Applied {
        events: Vec<EventEnvelope>,
        watermark: Seq,
    },
    /// A version-bearing command (a transition) was refused because the
    /// aggregate has moved past `expected_version`. Retryable ONLY by a
    /// caller that re-reads the current version and re-originates — this
    /// type never retries on its own, because guessing the next version
    /// would be racing whoever else moved it.
    StaleVersion {
        code: KernelErrorCode,
        message: String,
    },
    /// Any other refusal. Not retryable by resubmitting the identical
    /// envelope unchanged — the key is either permanently wrong for what it
    /// names, or the refusal is about something other than timing.
    Refused {
        code: KernelErrorCode,
        message: String,
    },
}

/// Reduce a raw [`KernelResult`] to an [`Outcome`].
///
/// `submit`'s own contract (`crates/gwk-kernel/src/submit.rs`'s
/// `route_of`, matched exhaustively with no wildcard arm) answers a
/// `SubmitCommand` request with exactly `CommandApplied` or `Error` —
/// never a projection, a blob, or a subscription result. The catch-all arm
/// below exists for that guarantee living in a DIFFERENT crate, not for an
/// expected case: it turns "this cannot happen today" into a reported
/// refusal instead of a panic if a future protocol addition ever proved it
/// wrong.
fn classify(result: KernelResult) -> Outcome {
    match result {
        KernelResult::CommandApplied {
            events, watermark, ..
        } => Outcome::Applied { events, watermark },
        KernelResult::Error { code, message, .. } if code == KernelErrorCode::StaleVersion => {
            Outcome::StaleVersion { code, message }
        }
        KernelResult::Error { code, message, .. } => Outcome::Refused { code, message },
        other => Outcome::Refused {
            code: KernelErrorCode::Schema,
            message: format!("SubmitCommand answered with an unexpected result: {other:?}"),
        },
    }
}

/// Mint, submit, and classify one command this host originated.
pub async fn submit(
    client: &mut KernelClient,
    command: &KernelCommand,
    key: IdempotencyKey,
    project: ProjectId,
    issued_at: Timestamp,
) -> Result<Outcome, KernelClientError> {
    let envelope = envelope::mint(command, key, project, issued_at);
    let result = client.submit(envelope).await?;
    Ok(classify(result))
}

/// Why one transcript record could not be fully originated.
///
/// Every variant is a value, never a panic — the seeded-malformed-transcript
/// verify rides [`Malformed`](Self::Malformed), and the wire-order contract
/// (`ingest::originate`'s doc) is why a refusal mid-record STOPS the record:
/// submitting an `IngestRecord` after its `RegisterDispatchNode` was refused
/// would attribute content to a node the kernel never accepted.
#[derive(Debug, thiserror::Error)]
pub enum OriginationError {
    /// The record itself was unusable; nothing was submitted.
    #[error("the transcript record could not be originated: {0}")]
    Malformed(#[from] TranscriptOriginationError),
    /// The kernel connection failed mid-record. The idempotency keys make
    /// the whole record safe to re-drive after a reconnect.
    #[error(transparent)]
    Client(#[from] KernelClientError),
    /// The kernel refused a command. `applied` says how many of the record's
    /// commands landed before it — with the pinned ordering, that is enough
    /// to know exactly which.
    #[error("{command_type} was refused ({code:?}): {message} ({applied} applied before it)")]
    Refused {
        command_type: &'static str,
        code: KernelErrorCode,
        message: String,
        applied: usize,
    },
    /// A stale transition re-read the node and the node was gone — a state
    /// only a competing writer can produce.
    #[error("dispatch node {0} vanished between a stale transition and its re-read")]
    NodeVanished(DispatchNodeId),
    /// The transition lost the version race twice — once against the
    /// original guess and once against a freshly-read version. Someone else
    /// is moving this node; re-driving blind would keep racing them.
    #[error("dispatch node {node} moved again past re-read version {reread_version}")]
    StaleAfterReread {
        node: DispatchNodeId,
        reread_version: u32,
    },
}

/// Originate one normalized transcript record end to end: build its
/// commands, submit each in the pinned order with its family's idempotency
/// key, and reduce the results. On success the returned outcomes are one
/// per submitted command, in submission order.
///
/// Per-family handling, concretely:
/// - **`IngestRecord`** — content-addressed key; a `StaleVersion` cannot
///   occur (the command carries no CAS) and is treated as the refusal it is.
/// - **`RegisterDispatchNode`** — keyed on the node id; same posture.
/// - **`TransitionDispatchNode`** — keyed on id + version. On
///   `StaleVersion`, the node's current version is re-read once and the
///   transition re-originated at it with a fresh key; a second stale answer
///   is a typed give-up, never a loop — see [`Outcome::StaleVersion`].
pub async fn originate_record(
    client: &mut KernelClient,
    record: TranscriptRecord,
    project: &ProjectId,
    issued_at: &Timestamp,
) -> Result<Vec<Outcome>, OriginationError> {
    // The ingest key needs the record's kind and body, which `originate`
    // consumes — derived first, while both are still ours to read.
    let ingest_key = ingest::ingest_key(record.kind, &record.body);
    let commands = ingest::originate(record)?;

    let mut outcomes = Vec::with_capacity(commands.len());
    for command in commands {
        let key = match &command {
            KernelCommand::IngestRecord { .. } => ingest_key.clone(),
            KernelCommand::RegisterDispatchNode {
                dispatch_node_id, ..
            } => crate::dispatch_node::register_key(dispatch_node_id),
            KernelCommand::TransitionDispatchNode {
                dispatch_node_id,
                expected_version,
                ..
            } => crate::dispatch_node::transition_key(dispatch_node_id, *expected_version),
            // `ingest::originate` produces exactly the three families above.
            // The guarantee lives in another module, so its violation is a
            // reported refusal here rather than a panic — the same posture
            // `classify` takes toward `submit`'s contract.
            other => {
                return Err(OriginationError::Refused {
                    command_type: other.command_type(),
                    code: KernelErrorCode::Schema,
                    message: "originate produced a command family with no key convention"
                        .to_owned(),
                    applied: outcomes.len(),
                });
            }
        };

        let outcome = submit(client, &command, key, project.clone(), issued_at.clone()).await?;
        match outcome {
            Outcome::Applied { .. } => outcomes.push(outcome),
            Outcome::StaleVersion { .. } => {
                let retried =
                    retry_stale_transition(client, &command, project, issued_at, outcomes.len())
                        .await?;
                outcomes.push(retried);
            }
            Outcome::Refused { code, message } => {
                return Err(OriginationError::Refused {
                    command_type: command.command_type(),
                    code,
                    message,
                    applied: outcomes.len(),
                });
            }
        }
    }
    Ok(outcomes)
}

/// The one sanctioned response to a stale transition: re-read the node's
/// version, re-originate at it, and accept exactly one more answer.
async fn retry_stale_transition(
    client: &mut KernelClient,
    command: &KernelCommand,
    project: &ProjectId,
    issued_at: &Timestamp,
    applied: usize,
) -> Result<Outcome, OriginationError> {
    let KernelCommand::TransitionDispatchNode {
        dispatch_node_id,
        to,
        ..
    } = command
    else {
        // A stale answer to a command with no CAS field is the kernel
        // disagreeing with its own contract; report it, don't guess.
        return Err(OriginationError::Refused {
            command_type: command.command_type(),
            code: KernelErrorCode::StaleVersion,
            message: "a command with no expected_version answered stale".to_owned(),
            applied,
        });
    };
    let Some(version) = client.dispatch_node_version(dispatch_node_id).await? else {
        return Err(OriginationError::NodeVanished(dispatch_node_id.clone()));
    };
    let retried = crate::dispatch_node::transition(dispatch_node_id.clone(), to.clone(), version);
    let key = crate::dispatch_node::transition_key(dispatch_node_id, version);
    match submit(client, &retried, key, project.clone(), issued_at.clone()).await? {
        outcome @ Outcome::Applied { .. } => Ok(outcome),
        Outcome::StaleVersion { .. } => Err(OriginationError::StaleAfterReread {
            node: dispatch_node_id.clone(),
            reread_version: version,
        }),
        Outcome::Refused { code, message } => Err(OriginationError::Refused {
            command_type: retried.command_type(),
            code,
            message,
            applied,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gwk_domain::CommandId;
    use gwk_domain::entity::DispatchNode;
    use gwk_domain::protocol::{
        ClientControl, FRAME_BODY_MAX_BYTES, FrameKind, KernelRequest, ProjectionRecord,
        ProtocolVersion, ServerControl,
    };
    use gwk_kernel::wire::frame::{Budget, Incoming, read_frame, write_frame};
    use serde_json::Value;

    /// A scripted stand-in for the kernel on a real Unix socket: answers the
    /// hello, then one scripted result per request, recording what each
    /// request WAS — the shape the tests assert order and keys against.
    ///
    /// One request at a time and no subscriptions, exactly the traffic
    /// `KernelClient` produces; anything else the client sends would hang
    /// the test's 10s timeout rather than silently pass.
    struct StubKernel {
        path: std::path::PathBuf,
        served: tokio::task::JoinHandle<Vec<ServedRequest>>,
    }

    /// What the stub saw: the command type and idempotency key of a submit,
    /// or the projection id of a read.
    #[derive(Debug, Clone, PartialEq)]
    enum ServedRequest {
        Submitted { command_type: String, key: String },
        ReadProjection { id: String },
    }

    impl StubKernel {
        fn start(script: Vec<KernelResult>) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "gwk-pty-host-stub-{}-{}",
                std::process::id(),
                // Distinct per test within one process.
                {
                    use std::sync::atomic::{AtomicU64, Ordering};
                    static N: AtomicU64 = AtomicU64::new(0);
                    N.fetch_add(1, Ordering::SeqCst)
                }
            ));
            std::fs::create_dir_all(&dir).expect("stub socket dir");
            let path = dir.join("gwk.sock");
            let listener = std::os::unix::net::UnixListener::bind(&path).expect("bind stub");
            listener.set_nonblocking(true).expect("nonblocking");
            let listener = tokio::net::UnixListener::from_std(listener).expect("tokio listener");

            let served = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut budget = Budget::new(usize::MAX / 2, usize::MAX / 2);
                let mut seen = Vec::new();

                // The hello.
                let Incoming::Frame(frame) =
                    read_frame(&mut stream, FRAME_BODY_MAX_BYTES, &mut budget)
                        .await
                        .expect("hello frame")
                else {
                    panic!("closed before the hello");
                };
                let ClientControl::Hello { .. } =
                    serde_json::from_slice(&frame.body).expect("decode hello")
                else {
                    panic!("first frame was not a hello");
                };
                let ack = ServerControl::HelloAck {
                    protocol_major: ProtocolVersion::V1,
                    protocol_minor: 0,
                    capabilities: Vec::new(),
                    sealed: false,
                    watermark: None,
                };
                write_frame(
                    &mut stream,
                    FrameKind::Json,
                    &serde_json::to_vec(&ack).expect("encode ack"),
                    &mut budget,
                )
                .await
                .expect("write ack");

                // One scripted answer per request; a clean close ends it.
                for result in script {
                    let incoming = read_frame(&mut stream, FRAME_BODY_MAX_BYTES, &mut budget)
                        .await
                        .expect("request frame");
                    let Incoming::Frame(frame) = incoming else {
                        panic!("the client closed with script left: {result:?}");
                    };
                    let ClientControl::Request {
                        request_id,
                        request,
                    } = serde_json::from_slice(&frame.body).expect("decode request")
                    else {
                        panic!("expected a request");
                    };
                    seen.push(match &request {
                        KernelRequest::SubmitCommand { envelope } => ServedRequest::Submitted {
                            command_type: envelope.command_type.clone(),
                            key: envelope.idempotency_key.as_str().to_owned(),
                        },
                        KernelRequest::GetProjection { id, .. } => {
                            ServedRequest::ReadProjection { id: id.clone() }
                        }
                        other => panic!("the driver sent an unexpected request: {other:?}"),
                    });
                    let response = ServerControl::Response {
                        request_id,
                        result: result.clone(),
                    };
                    write_frame(
                        &mut stream,
                        FrameKind::Json,
                        &serde_json::to_vec(&response).expect("encode response"),
                        &mut budget,
                    )
                    .await
                    .expect("write response");
                }
                seen
            });
            Self { path, served }
        }

        async fn finish(self) -> Vec<ServedRequest> {
            tokio::time::timeout(std::time::Duration::from_secs(10), self.served)
                .await
                .expect("the stub never finished its script")
                .expect("the stub panicked")
        }
    }

    fn applied() -> KernelResult {
        KernelResult::CommandApplied {
            command_id: CommandId::new("cmd-x"),
            events: Vec::new(),
            watermark: Seq::new(1),
        }
    }

    fn stale() -> KernelResult {
        KernelResult::Error {
            code: KernelErrorCode::StaleVersion,
            message: "expected 1, at 4".to_owned(),
            detail: None,
        }
    }

    fn node_projection(version: u32) -> KernelResult {
        KernelResult::Projection {
            record: ProjectionRecord::DispatchNode {
                dispatch_node: DispatchNode {
                    id: gwk_domain::DispatchNodeId::new("node-1"),
                    version,
                    parent_id: None,
                    attempt_id: None,
                    kind: "subagent".to_owned(),
                    state: "registered".to_owned(),
                    label: None,
                    created_at: Timestamp::new("2026-08-06T00:00:00Z"),
                    updated_at: Timestamp::new("2026-08-06T00:00:00Z"),
                },
            },
        }
    }

    fn record(fan_out: Option<crate::ingest::FanOutHint>) -> TranscriptRecord {
        TranscriptRecord {
            kind: gwk_domain::IngestionKind::Session,
            body: serde_json::json!({"text": "hello"}),
            fan_out,
        }
    }

    fn ts() -> Timestamp {
        Timestamp::new("2026-08-06T00:00:00Z")
    }

    #[tokio::test]
    async fn a_started_record_registers_before_it_ingests() {
        let stub = StubKernel::start(vec![applied(), applied()]);
        let mut client = KernelClient::connect(&stub.path).await.expect("connect");

        let outcomes = originate_record(
            &mut client,
            record(Some(crate::ingest::FanOutHint::Started {
                dispatch_node_id: gwk_domain::DispatchNodeId::new("node-1"),
                parent_id: None,
                attempt_id: None,
                kind: "subagent".to_owned(),
                label: None,
            })),
            &ProjectId::new("proj-1"),
            &ts(),
        )
        .await
        .expect("both commands apply");
        assert_eq!(outcomes.len(), 2);

        drop(client);
        let served = stub.finish().await;
        let types: Vec<&str> = served
            .iter()
            .map(|s| match s {
                ServedRequest::Submitted { command_type, .. } => command_type.as_str(),
                ServedRequest::ReadProjection { .. } => "get_projection",
            })
            .collect();
        assert_eq!(
            types,
            ["register_dispatch_node", "ingest_record"],
            "the pinned order: register first, so content never lands on an unregistered node"
        );
    }

    #[tokio::test]
    async fn a_stale_transition_re_reads_the_version_and_retries_once() {
        // ingest applies; the transition at version 1 is stale; the re-read
        // says 4; the retry at 4 applies.
        let stub = StubKernel::start(vec![applied(), stale(), node_projection(4), applied()]);
        let mut client = KernelClient::connect(&stub.path).await.expect("connect");

        let outcomes = originate_record(
            &mut client,
            record(Some(crate::ingest::FanOutHint::Transitioned {
                dispatch_node_id: gwk_domain::DispatchNodeId::new("node-1"),
                to: "finished".to_owned(),
                expected_version: 1,
            })),
            &ProjectId::new("proj-1"),
            &ts(),
        )
        .await
        .expect("the retry lands");
        assert_eq!(outcomes.len(), 2);

        drop(client);
        let served = stub.finish().await;
        assert_eq!(served.len(), 4);
        assert_eq!(
            served[3],
            ServedRequest::Submitted {
                command_type: "transition_dispatch_node".to_owned(),
                key: "transition_dispatch_node:node-1:4".to_owned(),
            },
            "the retry must carry the re-read version in body AND key"
        );
    }

    #[tokio::test]
    async fn a_stale_retry_that_is_stale_again_gives_up_typed() {
        let stub = StubKernel::start(vec![applied(), stale(), node_projection(4), stale()]);
        let mut client = KernelClient::connect(&stub.path).await.expect("connect");

        let error = originate_record(
            &mut client,
            record(Some(crate::ingest::FanOutHint::Transitioned {
                dispatch_node_id: gwk_domain::DispatchNodeId::new("node-1"),
                to: "finished".to_owned(),
                expected_version: 1,
            })),
            &ProjectId::new("proj-1"),
            &ts(),
        )
        .await
        .expect_err("a second stale answer is a give-up, not a loop");
        assert!(
            matches!(
                error,
                OriginationError::StaleAfterReread {
                    reread_version: 4,
                    ..
                }
            ),
            "{error:?}"
        );
        drop(client);
        stub.finish().await;
    }

    /// The verify command's named end-to-end case: a seeded malformed
    /// transcript makes `IngestRecord` origination fail TYPED through the
    /// whole driver — and nothing reaches the kernel.
    #[tokio::test]
    async fn a_malformed_transcript_fails_the_whole_driver_typed_and_submits_nothing() {
        let stub = StubKernel::start(Vec::new());
        let mut client = KernelClient::connect(&stub.path).await.expect("connect");

        let error = originate_record(
            &mut client,
            TranscriptRecord {
                kind: gwk_domain::IngestionKind::Session,
                body: Value::String("not an object".to_owned()),
                fan_out: None,
            },
            &ProjectId::new("proj-1"),
            &ts(),
        )
        .await
        .expect_err("a malformed body must refuse, not submit");
        assert!(
            matches!(
                error,
                OriginationError::Malformed(TranscriptOriginationError::BodyNotAnObject("string"))
            ),
            "{error:?}"
        );

        drop(client);
        let served = stub.finish().await;
        assert!(
            served.is_empty(),
            "nothing may reach the kernel for a record that never originated: {served:?}"
        );
    }

    #[tokio::test]
    async fn a_refused_register_stops_the_record_before_its_ingest() {
        let stub = StubKernel::start(vec![KernelResult::Error {
            code: KernelErrorCode::Authority,
            message: "no".to_owned(),
            detail: None,
        }]);
        let mut client = KernelClient::connect(&stub.path).await.expect("connect");

        let error = originate_record(
            &mut client,
            record(Some(crate::ingest::FanOutHint::Started {
                dispatch_node_id: gwk_domain::DispatchNodeId::new("node-1"),
                parent_id: None,
                attempt_id: None,
                kind: "subagent".to_owned(),
                label: None,
            })),
            &ProjectId::new("proj-1"),
            &ts(),
        )
        .await
        .expect_err("a refused register must stop the record");
        assert!(
            matches!(
                &error,
                OriginationError::Refused { command_type, applied: 0, .. }
                    if *command_type == "register_dispatch_node"
            ),
            "{error:?}"
        );

        drop(client);
        let served = stub.finish().await;
        assert_eq!(
            served.len(),
            1,
            "the ingest must never follow a refused register: {served:?}"
        );
    }

    #[test]
    fn an_applied_result_classifies_as_applied() {
        let outcome = classify(KernelResult::CommandApplied {
            command_id: CommandId::new("cmd-1"),
            events: Vec::new(),
            watermark: Seq::new(9),
        });
        assert!(matches!(
            outcome,
            Outcome::Applied { watermark, .. } if watermark == Seq::new(9)
        ));
    }

    #[test]
    fn a_stale_version_refusal_classifies_separately_from_every_other_refusal() {
        let stale = classify(KernelResult::Error {
            code: KernelErrorCode::StaleVersion,
            message: "old".to_owned(),
            detail: None,
        });
        assert!(matches!(stale, Outcome::StaleVersion { .. }));

        let other = classify(KernelResult::Error {
            code: KernelErrorCode::Authority,
            message: "no".to_owned(),
            detail: None,
        });
        assert!(matches!(
            other,
            Outcome::Refused {
                code: KernelErrorCode::Authority,
                ..
            }
        ));
    }

    #[test]
    fn a_result_no_submit_command_answer_can_ever_be_classifies_as_a_refusal_not_a_panic() {
        let outcome = classify(KernelResult::Watermark { watermark: None });
        assert!(matches!(
            outcome,
            Outcome::Refused {
                code: KernelErrorCode::Schema,
                ..
            }
        ));
    }
}
