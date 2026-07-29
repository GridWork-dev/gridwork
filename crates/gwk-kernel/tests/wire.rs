//! The daemon over a real Unix socket, answering a real kernel.
//!
//! The unit cases prove the codec and the socket rules in isolation; this suite
//! proves they compose — a client that speaks the framing, completes the
//! handshake, and asks a question gets an answer derived from the database
//! rather than from constants.
//!
//! The read cases are worth more than they look. A projection query is a static
//! string per table, so nothing but a live PostgreSQL with the real DDL behind
//! it can tell a correct one from a plausible one: a wrong column name, a wrong
//! cursor, or a record shape that no longer matches its contract type all
//! compile and all pass a unit test.

mod common;

use common::*;
use gwk_domain::ids::Seq;
use gwk_domain::protocol::{
    CONNECTION_EGRESS_BYTES_PER_WINDOW, CONNECTION_INGRESS_BYTES_PER_WINDOW, CONTRACT_VERSION,
    FRAME_BODY_MAX_BYTES, FrameKind, KernelErrorCode, KernelResult,
    MAX_SUBSCRIPTIONS_PER_CONNECTION, ProjectionKind, ProjectionRecord, ProtocolVersion,
    SUBSCRIPTION_POLL_SECS, ServerControl,
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

fn daemon_for(store: PgEventStore) -> Daemon {
    Daemon::new(store, TEST_REVISION.to_owned()).expect("daemon")
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
            budget: Budget::new(
                CONNECTION_INGRESS_BYTES_PER_WINDOW,
                CONNECTION_EGRESS_BYTES_PER_WINDOW,
            ),
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

/// A daemon on its own socket with a client already through the handshake.
struct Served {
    client: Client,
    dir: PathBuf,
    serving: tokio::task::JoinHandle<()>,
}

impl Served {
    async fn open(store: PgEventStore, tag: &str) -> Self {
        let dir = runtime_dir(tag);
        let path = dir.join("gwk.sock");
        let listener = Listener::bind(&path).await.expect("bind");
        let daemon = Arc::new(daemon_for(store));
        let serving = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let _ = serve_stream(&daemon, stream).await;
            listener.remove();
        });
        let (client, _) = Client::connect(&path).await;
        Self {
            client,
            dir,
            serving,
        }
    }

    /// Hang up, let the connection task finish, and take the socket with it.
    async fn close(self) {
        drop(self.client);
        self.serving.await.expect("join");
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The real accept loop, serving as many connections as a test opens.
///
/// [`Served`] takes exactly one, which is all request/response needs. A
/// subscription needs two: the client watching the log must not be the client
/// appending to it, or its own submit response and the batch that submit caused
/// arrive on one wire with nothing in the protocol deciding their order. It also
/// gets the notification listener, which `serve_stream` does not start.
struct Running {
    dir: PathBuf,
    path: PathBuf,
    stop: tokio::sync::oneshot::Sender<()>,
    serving: tokio::task::JoinHandle<()>,
}

impl Running {
    async fn open(store: PgEventStore, tag: &str) -> Self {
        let dir = runtime_dir(tag);
        let path = dir.join("gwk.sock");
        let listener = Listener::bind(&path).await.expect("bind");
        let daemon = Arc::new(daemon_for(store));
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let serving = tokio::spawn(async move {
            let _ = gwk_kernel::wire::serve::run(listener, daemon, async move {
                let _ = stopped.await;
            })
            .await;
        });
        Self {
            dir,
            path,
            stop,
            serving,
        }
    }

    async fn client(&self) -> Client {
        Client::connect(&self.path).await.0
    }

    /// Stop accepting and drain. Every client must be dropped first — a live one
    /// makes the drain wait out its timeout rather than finish.
    async fn close(self) {
        let _ = self.stop.send(());
        self.serving.await.expect("join");
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Subscribe from a known point, so a case that is about later events is not
/// handed the whole log first.
fn subscribe_from(cursor: Seq) -> String {
    format!(
        r#"{{"type":"subscribe_events","cursor":"{}"}}"#,
        cursor.value()
    )
}

/// The watermark, as the client sees it.
async fn watermark_of(client: &mut Client, id: &str) -> Seq {
    match client.ask(id, r#"{"type":"watermark"}"#).await {
        KernelResult::Watermark { watermark } => watermark.expect("genesis is in the log"),
        other => panic!("{other:?}"),
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
    let daemon = Arc::new(daemon_for(store));

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

/// The id a page continues from, for whichever projection this is.
fn record_key(record: &ProjectionRecord) -> String {
    let json = serde_json::to_value(record).expect("serialize");
    let body = &json[record.kind().as_str()];
    body.get("id")
        .or_else(|| body.get("orchestrator_id"))
        .and_then(serde_json::Value::as_str)
        .expect("every projection record carries the key it is paged by")
        .to_owned()
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn every_projection_a_client_can_name_comes_back_through_the_wire() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wire_projections", 8).await;
    // Every table gets rows on purpose. A query against an EMPTY table returns
    // no rows whether it is right or wrong, so an unpopulated projection is a
    // case that passes without having tested anything.
    populate(&store).await;
    let mut served = Served::open(store, "projections").await;

    for kind in ProjectionKind::ALL {
        let tag = kind.as_str();
        match served
            .client
            .ask(
                &format!("r-{tag}"),
                &format!(r#"{{"type":"list_projection","projection":"{tag}"}}"#),
            )
            .await
        {
            KernelResult::ProjectionPage { records, .. } => {
                assert!(!records.is_empty(), "{tag} came back empty");
                for record in &records {
                    // The round trip through the contract type happens server
                    // side; this asserts the server answered about the table it
                    // was asked about, which a copy-paste in the query table
                    // would silently get wrong.
                    assert_eq!(record.kind(), *kind, "{tag} answered with another table");
                }
            }
            other => panic!("{tag}: {other:?}"),
        }
    }

    served.close().await;
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_page_at_a_time_walks_a_projection_exactly_once() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wire_paging", 8).await;
    // Ids chosen to differ by punctuation and case, because that is precisely
    // where a locale collation puts two keys in an order its `>` disagrees
    // with — and a cursor walk across that boundary loses a row.
    let ids = ["t-a", "t-A", "t_a", "t-a-1", "t-a1", "tA", "ta"];
    for id in ids {
        apply(&store, id, task(id)).await;
    }
    let mut served = Served::open(store, "paging").await;

    let mut seen: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    // One row per page: the boundary is crossed between every adjacent pair,
    // which is the only way to exercise all of them.
    for round in 0..ids.len() + 2 {
        let request = match &cursor {
            Some(c) => format!(
                r#"{{"type":"list_projection","projection":"task","cursor":"{c}","limit":1}}"#
            ),
            None => r#"{"type":"list_projection","projection":"task","limit":1}"#.to_owned(),
        };
        match served.client.ask(&format!("r-{round}"), &request).await {
            KernelResult::ProjectionPage {
                records,
                next_cursor,
            } => {
                seen.extend(records.iter().map(record_key));
                match next_cursor {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }
            other => panic!("{other:?}"),
        }
    }

    let mut expected: Vec<String> = ids.iter().map(|s| (*s).to_owned()).collect();
    expected.sort_unstable();
    let mut got = seen.clone();
    got.sort_unstable();
    // Exactly once: `sort` then compare catches a repeat, and the length check
    // that a dedup would have hidden catches it too.
    assert_eq!(
        got, expected,
        "the walk did not see every task exactly once"
    );
    assert_eq!(seen.len(), ids.len(), "a row was delivered twice");
    // And the walk was in the order the cursor claimed, not merely complete.
    assert_eq!(seen, expected, "pages did not arrive in key order");

    served.close().await;
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn one_record_by_id_is_the_same_record_the_page_delivered() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wire_get", 8).await;
    apply(&store, "t-1", task("t-1")).await;
    let mut served = Served::open(store, "get").await;

    let from_page = match served
        .client
        .ask(
            "r-list",
            r#"{"type":"list_projection","projection":"task"}"#,
        )
        .await
    {
        KernelResult::ProjectionPage { mut records, .. } => records.pop().expect("one task"),
        other => panic!("{other:?}"),
    };

    match served
        .client
        .ask(
            "r-get",
            r#"{"type":"get_projection","projection":"task","id":"t-1"}"#,
        )
        .await
    {
        // The get and the list run the same query with different bindings, so
        // a difference here means one of the two bindings is wrong.
        KernelResult::Projection { record } => assert_eq!(record, from_page),
        other => panic!("{other:?}"),
    }

    match served
        .client
        .ask(
            "r-missing",
            r#"{"type":"get_projection","projection":"task","id":"t-nope"}"#,
        )
        .await
    {
        // Absent is an ANSWER. A client that asked whether a task exists gets
        // told, and the connection carries on.
        KernelResult::Error { code, .. } => assert_eq!(code, KernelErrorCode::NotFound),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        served.client.ask("r-after", r#"{"type":"health"}"#).await,
        KernelResult::Health { .. }
    ));

    served.close().await;
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_log_reads_back_from_a_cursor_in_the_order_it_was_written() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wire_events", 8).await;
    for i in 0..6 {
        apply(&store, &format!("t-{i}"), task(&format!("t-{i}"))).await;
    }
    let mut served = Served::open(store, "events").await;

    let watermark = match served.client.ask("r-wm", r#"{"type":"watermark"}"#).await {
        KernelResult::Watermark { watermark } => watermark.expect("events exist"),
        other => panic!("{other:?}"),
    };

    let mut collected: Vec<u64> = Vec::new();
    let mut cursor: Option<u64> = None;
    for round in 0..10 {
        let request = match cursor {
            Some(c) => format!(r#"{{"type":"read_events","cursor":"{c}","limit":2}}"#),
            None => r#"{"type":"read_events","limit":2}"#.to_owned(),
        };
        match served.client.ask(&format!("r-e{round}"), &request).await {
            KernelResult::Events {
                events,
                cursor: last,
                ..
            } => {
                if events.is_empty() {
                    // The cursor a page reports is the last one DELIVERED, so an
                    // empty page reports nothing and the walk is over.
                    assert!(last.is_none());
                    break;
                }
                collected.extend(events.iter().map(|e| e.global_sequence.value()));
                // Reported against delivered, not against requested: a client
                // resuming from this value resumes from what it actually got.
                assert_eq!(
                    last.expect("a non-empty page reports its last sequence"),
                    events.last().expect("non-empty").global_sequence
                );
                cursor = Some(last.expect("checked").value());
            }
            other => panic!("{other:?}"),
        }
    }

    assert!(!collected.is_empty());
    assert_eq!(*collected.last().expect("non-empty"), watermark.value());
    // Strictly ascending, with no sequence delivered twice.
    assert!(
        collected.windows(2).all(|w| w[0] < w[1]),
        "the log came back out of order or repeated: {collected:?}"
    );

    served.close().await;
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_command_submitted_over_the_wire_lands_once_however_often_it_is_sent() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wire_submit", 8).await;
    let mut served = Served::open(store, "submit").await;

    let envelope = serde_json::to_string(&envelope("k-1", &task("t-1"))).expect("serialize");
    let first = match served
        .client
        .ask(
            "r-submit",
            &format!(r#"{{"type":"submit_command","envelope":{envelope}}}"#),
        )
        .await
    {
        KernelResult::CommandApplied { events, .. } => events,
        other => panic!("{other:?}"),
    };
    assert_eq!(first.len(), 1);

    // The identical request again. Idempotency is enforced under the writer
    // lock in `submit`, not out here — this asserts the wire does not undo it
    // by, say, rebuilding the envelope on the way through.
    let again = match served
        .client
        .ask(
            "r-submit-2",
            &format!(r#"{{"type":"submit_command","envelope":{envelope}}}"#),
        )
        .await
    {
        KernelResult::CommandApplied { events, .. } => events,
        other => panic!("{other:?}"),
    };
    assert_eq!(again, first, "a retry appended a second event");

    match served
        .client
        .ask(
            "r-check",
            r#"{"type":"list_projection","projection":"task"}"#,
        )
        .await
    {
        KernelResult::ProjectionPage { records, .. } => assert_eq!(records.len(), 1),
        other => panic!("{other:?}"),
    }

    served.close().await;
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_subscription_delivers_what_the_log_gains_after_it_started() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wire_subscribe", 8).await;
    let served = Running::open(store, "subscribe").await;

    let mut watcher = served.client().await;
    let mut appender = served.client().await;

    // From the current watermark, so what arrives can only be what this test
    // appended — a subscription from the beginning would deliver the log first
    // and prove nothing about live delivery.
    let watermark = watermark_of(&mut watcher, "r-wm").await;
    match watcher.ask("r-sub", &subscribe_from(watermark)).await {
        // The acknowledgement echoes where the stream starts, and it arrives
        // BEFORE any batch: the writer drains responses first, and the
        // subscription does not start until this is queued.
        KernelResult::Subscribed { cursor } => assert_eq!(cursor, Some(watermark)),
        other => panic!("{other:?}"),
    }

    let envelope = serde_json::to_string(&envelope("k-1", &task("t-1"))).expect("serialize");
    match appender
        .ask(
            "r-submit",
            &format!(r#"{{"type":"submit_command","envelope":{envelope}}}"#),
        )
        .await
    {
        KernelResult::CommandApplied { .. } => {}
        other => panic!("{other:?}"),
    }

    // Well inside the poll interval, which is the assertion that matters: this
    // arrived because the append notified, not because a timer came round.
    let batch = tokio::time::timeout(std::time::Duration::from_secs(3), watcher.recv())
        .await
        .expect("a notified subscription delivers without waiting for the poll")
        .expect("the connection stayed open");
    match batch {
        ServerControl::EventBatch {
            request_id,
            events,
            cursor,
        } => {
            // The id of the subscription, not of the submit: an unsolicited
            // frame is still matched to the request that asked for the stream.
            assert_eq!(request_id.as_str(), "r-sub");
            assert!(!events.is_empty(), "a batch with no events");
            assert!(
                events.iter().all(|e| e.global_sequence > watermark),
                "the stream replayed what the cursor had already covered"
            );
            // What the consumer actually received, so resuming from it is
            // gap-free and repeat-free.
            assert_eq!(
                cursor,
                events.last().expect("non-empty").global_sequence,
                "the batch cursor is not its last event"
            );
        }
        other => panic!("{other:?}"),
    }

    drop(watcher);
    drop(appender);
    served.close().await;
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_subscription_that_never_hears_a_notification_still_catches_up() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wire_poll", 8).await;
    // `Served` serves through `serve_stream`, which never starts the notification
    // listener — so this is the lost-notification case with the notification
    // permanently lost, and the poll is the only thing that can deliver. It is
    // also what makes one connection enough here: nothing can push a batch until
    // long after the submit has been answered.
    let mut served = Served::open(store, "poll").await;

    let watermark = watermark_of(&mut served.client, "r-wm").await;
    match served.client.ask("r-sub", &subscribe_from(watermark)).await {
        KernelResult::Subscribed { .. } => {}
        other => panic!("{other:?}"),
    }

    let envelope = serde_json::to_string(&envelope("k-1", &task("t-1"))).expect("serialize");
    match served
        .client
        .ask(
            "r-submit",
            &format!(r#"{{"type":"submit_command","envelope":{envelope}}}"#),
        )
        .await
    {
        KernelResult::CommandApplied { .. } => {}
        other => panic!("{other:?}"),
    }

    // Bounded, which is the whole claim: a subscriber whose notification was
    // lost waits an interval, not forever.
    let batch = tokio::time::timeout(
        std::time::Duration::from_secs(SUBSCRIPTION_POLL_SECS + 5),
        served.client.recv(),
    )
    .await
    .expect("the poll delivered what the lost notification did not")
    .expect("the connection stayed open");
    match batch {
        ServerControl::EventBatch { events, cursor, .. } => {
            assert!(events.iter().all(|e| e.global_sequence > watermark));
            assert_eq!(cursor, events.last().expect("non-empty").global_sequence);
        }
        other => panic!("{other:?}"),
    }

    served.close().await;
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_connection_may_not_hold_more_subscriptions_than_its_cap() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wire_subcap", 8).await;
    let mut served = Served::open(store, "subcap").await;

    // All of them start at the watermark, so none has anything to deliver: this
    // case is about how many streams may exist, and a batch arriving mid-test
    // would be a different one.
    let watermark = watermark_of(&mut served.client, "r-wm").await;
    for i in 0..MAX_SUBSCRIPTIONS_PER_CONNECTION {
        match served
            .client
            .ask(&format!("r-s{i}"), &subscribe_from(watermark))
            .await
        {
            KernelResult::Subscribed { .. } => {}
            other => panic!("subscription {i}: {other:?}"),
        }
    }

    match served
        .client
        .ask("r-over", &subscribe_from(watermark))
        .await
    {
        KernelResult::Error { code, message, .. } => {
            assert_eq!(code, KernelErrorCode::Overloaded);
            assert!(
                message.contains(&MAX_SUBSCRIPTIONS_PER_CONNECTION.to_string()),
                "the refusal does not say what the cap is: {message}"
            );
        }
        other => panic!("{other:?}"),
    }

    // One request refused, nothing else touched — neither the connection nor the
    // eight subscriptions already on it.
    assert!(matches!(
        served.client.ask("r-after", r#"{"type":"health"}"#).await,
        KernelResult::Health { .. }
    ));

    served.close().await;
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
    let daemon = Arc::new(daemon_for(store));

    let serving = tokio::spawn({
        let daemon = Arc::clone(&daemon);
        async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let _ = serve_stream(&daemon, stream).await;
            listener.remove();
        }
    });

    let (mut client, _) = Client::connect(&path).await;
    // A legal address for a blob that does not exist — the refusal happens
    // before anything is looked up, which is the point: the request is not
    // served, rather than served and found wanting.
    let address = format!("sha256:{}", "0".repeat(64));
    match client
        .ask(
            "r-blob",
            &format!(r#"{{"type":"blob_stat","address":"{address}"}}"#),
        )
        .await
    {
        KernelResult::Error { code, message, .. } => {
            assert_eq!(code, KernelErrorCode::Validation);
            // By name: a client learns which request it was, and the refusal
            // does not read as "your request was malformed".
            assert!(message.contains("blob_stat"), "{message}");
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
    let daemon = Arc::new(daemon_for(store));

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
