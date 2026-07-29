//! The daemon over a real Unix socket, answering a real sealed kernel.
//!
//! The unit cases prove the codec and the socket rules in isolation; this suite
//! proves they compose — a client that speaks the framing, completes the
//! handshake, and asks the four questions a SEALED daemon can answer gets
//! answers derived from the database rather than from constants.

mod common;

use common::*;
use gwk_domain::protocol::{
    CONNECTION_EGRESS_MAX_BYTES, CONNECTION_INGRESS_MAX_BYTES, CONTRACT_VERSION,
    FRAME_BODY_MAX_BYTES, FrameKind, KernelErrorCode, KernelResult, ProtocolVersion, ServerControl,
};
use gwk_kernel::store::PgEventStore;
use gwk_kernel::wire::frame::{Budget, Incoming, read_frame, write_frame};
use gwk_kernel::wire::listen::Listener;
use gwk_kernel::wire::serve::{Daemon, serve_stream};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::UnixStream;

/// A private runtime directory, as the daemon requires of its socket's parent.
fn runtime_dir(tag: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("gwk-wire-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create runtime dir");
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).expect("chmod");
    dir
}

async fn daemon_for(store: PgEventStore) -> Daemon {
    let epoch = gwk_kernel::claim_epoch(store.pool()).await.expect("epoch");
    Daemon::new(
        store,
        TEST_REVISION.to_owned(),
        gwk_domain::ids::WriterEpoch::new(epoch as u64),
    )
    .expect("daemon")
}

/// A client that speaks the wire: handshake, then one request per call.
struct Client {
    stream: UnixStream,
    budget: Budget,
}

impl Client {
    async fn connect(path: &std::path::Path) -> (Self, ServerControl) {
        let mut client = Self {
            stream: UnixStream::connect(path).await.expect("connect"),
            budget: Budget::new(CONNECTION_INGRESS_MAX_BYTES, CONNECTION_EGRESS_MAX_BYTES),
        };
        client
            .send(r#"{"type":"hello","protocol_major":1,"protocol_minor":0,"capabilities":[]}"#)
            .await;
        let ack = client.recv().await.expect("the daemon acked");
        (client, ack)
    }

    async fn send(&mut self, raw: &str) {
        write_frame(
            &mut self.stream,
            FrameKind::Json,
            raw.as_bytes(),
            &mut self.budget,
        )
        .await
        .expect("write");
    }

    async fn recv(&mut self) -> Option<ServerControl> {
        match read_frame(&mut self.stream, FRAME_BODY_MAX_BYTES, &mut self.budget)
            .await
            .expect("read")
        {
            Incoming::Frame(frame) => {
                Some(serde_json::from_slice(&frame.body).expect("decode the answer"))
            }
            Incoming::Closed => None,
        }
    }

    async fn ask(&mut self, id: &str, request: &str) -> KernelResult {
        self.send(&format!(
            r#"{{"type":"request","request_id":"{id}","request":{request}}}"#
        ))
        .await;
        match self.recv().await.expect("a response") {
            ServerControl::Response { request_id, result } => {
                // Every response carries the id it answers: a client with two
                // requests in flight has nothing else to match them by.
                assert_eq!(request_id.as_str(), id);
                result
            }
            other => panic!("{other:?}"),
        }
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_sealed_daemon_answers_the_whole_surface_it_promises() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_sealed_store(&maintenance, "wire_sealed", 8).await;
    let dir = runtime_dir("sealed");
    let path = dir.join("gwk.sock");
    let listener = Listener::bind(&path).await.expect("bind");
    let daemon = Arc::new(daemon_for(store).await);

    let serving = tokio::spawn({
        let daemon = Arc::clone(&daemon);
        async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let _ = serve_stream(&daemon, stream).await;
            listener.remove();
        }
    });

    let (mut client, ack) = Client::connect(&path).await;
    match ack {
        ServerControl::HelloAck {
            protocol_major,
            sealed,
            capabilities,
            ..
        } => {
            assert_eq!(protocol_major, ProtocolVersion::V1);
            // The ack tells a client it is sealed before its first request, so
            // it never has to discover that by being refused.
            assert!(sealed);
            assert!(capabilities.is_empty());
        }
        other => panic!("{other:?}"),
    }

    // A sealed kernel is READY. It admits no business command, which is a
    // different thing from being unhealthy — conflating them would fail a
    // health check for every deployment between genesis and cutover.
    assert_eq!(
        client.ask("r-health", r#"{"type":"health"}"#).await,
        KernelResult::Health {
            ready: true,
            sealed: true
        }
    );

    match client.ask("r-status", r#"{"type":"status"}"#).await {
        KernelResult::Status {
            sealed,
            contract_version,
            public_revision,
            watermark,
            ..
        } => {
            assert!(sealed);
            assert_eq!(contract_version, CONTRACT_VERSION);
            assert_eq!(public_revision, TEST_REVISION);
            assert!(watermark.is_some(), "genesis is in the log");
        }
        other => panic!("{other:?}"),
    }

    let watermark = match client.ask("r-wm", r#"{"type":"watermark"}"#).await {
        KernelResult::Watermark { watermark } => watermark.expect("genesis is in the log"),
        other => panic!("{other:?}"),
    };

    match client.ask("r-sealed", r#"{"type":"verify_sealed"}"#).await {
        KernelResult::SealedVerification {
            sealed,
            genesis_watermark,
            event_count,
            ..
        } => {
            assert!(sealed);
            // The proof: ONE event, at whatever sequence the database assigned
            // it. The count and the sequence are different questions and this
            // asserts both rather than assuming genesis sits at 1.
            assert_eq!(event_count.value(), 1);
            assert_eq!(genesis_watermark, watermark);
        }
        other => panic!("{other:?}"),
    }

    drop(client);
    serving.await.expect("join");
    let _ = std::fs::remove_dir_all(&dir);
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_request_this_layer_does_not_serve_is_refused_by_name() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_sealed_store(&maintenance, "wire_unserved", 8).await;
    let dir = runtime_dir("unserved");
    let path = dir.join("gwk.sock");
    let listener = Listener::bind(&path).await.expect("bind");
    let daemon = Arc::new(daemon_for(store).await);

    let serving = tokio::spawn({
        let daemon = Arc::clone(&daemon);
        async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let _ = serve_stream(&daemon, stream).await;
            listener.remove();
        }
    });

    let (mut client, _) = Client::connect(&path).await;
    match client
        .ask("r-events", r#"{"type":"read_events","limit":5}"#)
        .await
    {
        KernelResult::Error { code, message, .. } => {
            assert_eq!(code, KernelErrorCode::Validation);
            // By name: a client learns which request it was, and the refusal
            // does not read as "your request was malformed".
            assert!(message.contains("read_events"), "{message}");
        }
        other => panic!("{other:?}"),
    }
    // And the connection is still usable afterwards — a refusal is a value.
    assert!(matches!(
        client.ask("r-after", r#"{"type":"health"}"#).await,
        KernelResult::Health { .. }
    ));

    drop(client);
    serving.await.expect("join");
    let _ = std::fs::remove_dir_all(&dir);
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_accept_loop_stops_taking_work_and_takes_its_socket_with_it() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_sealed_store(&maintenance, "wire_drain", 8).await;
    let dir = runtime_dir("drain");
    let path = dir.join("gwk.sock");
    let listener = Listener::bind(&path).await.expect("bind");
    let daemon = Arc::new(daemon_for(store).await);

    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let running = tokio::spawn(gwk_kernel::wire::serve::run(listener, daemon, async move {
        let _ = stopped.await;
    }));

    // One live client, mid-session, to prove the drain waits for it.
    let (mut client, _) = Client::connect(&path).await;
    assert!(matches!(
        client.ask("r-1", r#"{"type":"health"}"#).await,
        KernelResult::Health { .. }
    ));

    stop.send(()).expect("signal shutdown");
    // The client hanging up is what lets the drain finish; without this the
    // 30-second timeout would be doing the work instead, which is the path this
    // case is NOT testing.
    drop(client);
    running.await.expect("join").expect("run");

    // The socket goes with it. A daemon that left its socket behind would make
    // the next start take the stale-takeover path for no reason.
    assert!(!path.exists(), "shutdown left the socket behind");
    // And nothing can connect any more.
    assert!(UnixStream::connect(&path).await.is_err());

    let _ = std::fs::remove_dir_all(&dir);
    drop_database(&maintenance, &name).await;
}
