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

use base64::prelude::{BASE64_STANDARD, Engine as _};
use common::*;
use gwk_domain::KernelCommand;
use gwk_domain::blob::BLOB_CHUNK_BYTES;
use gwk_domain::ids::Seq;
use gwk_domain::port::EventStore;
use gwk_domain::protocol::{
    CONNECTION_EGRESS_BYTES_PER_WINDOW, CONNECTION_INGRESS_BYTES_PER_WINDOW, CONTRACT_VERSION,
    ClientControl, FRAME_BODY_MAX_BYTES, KernelErrorCode, KernelRequest, KernelResult,
    MAX_SUBSCRIPTIONS_PER_CONNECTION, PTY_CONTROL_CAPABILITY, PTY_INPUT_CAPABILITY,
    PTY_START_CAPABILITY, ProjectionKind, ProjectionRecord, ProtocolVersion, PtyDeliveryResult,
    PtyInputData, SLOW_CONSUMER_TIMEOUT_SECS, SUBSCRIPTION_POLL_SECS, ServerControl,
};
use gwk_kernel::store::connect_pool;
use gwk_kernel::wire::frame::{Budget, Incoming, read_frame};
use gwk_kernel::wire::listen::Listener;
use gwk_kernel::wire::serve::serve_stream;
use std::sync::Arc;
use tokio::net::UnixStream;

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_sealed_daemon_answers_the_whole_surface_it_promises() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_sealed_store(&maintenance, "wire_sealed", 8).await;
    let dir = runtime_dir("sealed");
    let path = dir.join("gwk.sock");
    let listener = Listener::bind(&path).await.expect("bind");
    let (daemon, blobs) = daemon_for(store, "sealed").await;
    let daemon = Arc::new(daemon);

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
    let _ = std::fs::remove_dir_all(&blobs);
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

    // Two outcomes now, and the test asserts WHICH one each kind gets rather
    // than accepting either. A kind V1 does not serve is refused by policy; a
    // kind it does serve answers with rows. Collapsing these into "did not
    // panic" would let a served projection silently become unserved.
    let mut refused: Vec<&str> = Vec::new();
    for kind in ProjectionKind::ALL {
        let tag = kind.as_str();
        let answer = served
            .client
            .ask(
                &format!("r-{tag}"),
                &format!(r#"{{"type":"list_projection","projection":"{tag}"}}"#),
            )
            .await;
        if !kind.served_in_v1() {
            // Refused by POLICY, and the code is asserted: falling through to
            // `projection_page` would also fail here, as a Storage/Config error
            // claiming the table is missing, and the two must not read alike.
            match answer {
                KernelResult::Error { code, message, .. } => {
                    assert_eq!(
                        code,
                        KernelErrorCode::UnsupportedVersion,
                        "{tag} was refused with the wrong code"
                    );
                    assert!(
                        message.contains(tag),
                        "{tag} refusal does not name the projection: {message}"
                    );
                }
                other => panic!("{tag} should be refused by policy, got {other:?}"),
            }
            refused.push(tag);
            continue;
        }
        match answer {
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
    // By name, not by count — see the checkpoint invariant this mirrors.
    assert_eq!(
        refused,
        [
            "context_release",
            "context_observation",
            "context_finalization"
        ],
        "the set of projections the wire refuses has moved"
    );

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
                watermark,
                served_at,
            } => {
                // Every page carries how far the projector had applied. The
                // log is non-empty by construction here — these tasks were
                // appended above — so absent is a real failure, not an empty
                // log.
                assert!(
                    watermark.is_some(),
                    "page {round} carried no watermark against a non-empty log"
                );
                // And the clock it was served at, from the database rather
                // than this process — a client folding a day window against
                // its own wall clock is comparing to a boundary the rows never
                // agreed to. Asserted against a live kernel because that is
                // the only place the DB clock is real.
                let served_at =
                    served_at.unwrap_or_else(|| panic!("page {round} carried no served_at"));
                assert!(
                    served_at.as_str().contains('T'),
                    "page {round} served_at is not a timestamp: {served_at:?}"
                );
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
async fn a_frame_the_codec_refuses_takes_down_its_own_connection_and_no_other() {
    use tokio::io::AsyncWriteExt as _;

    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wire_strict", 8).await;
    let served = Running::open(store, "strict").await;

    // The codec's refusals are certified in isolation by the unit suite. What
    // only the accept loop can answer is what a refusal COSTS: the daemon serves
    // every connection in its own task and the comment there claims a malformed
    // frame from one client must not take down the others. That is this case.
    let mut bystander = served.client().await;
    assert!(matches!(
        bystander.ask("r-before", r#"{"type":"health"}"#).await,
        KernelResult::Health { .. }
    ));

    // A length prefix past the frame bound, and nothing else — refused from the
    // five header bytes, before a body is read or allocated. Written raw because
    // `write_frame` would not produce it.
    let mut liar = UnixStream::connect(&served.path).await.expect("connect");
    liar.write_all(&(FRAME_BODY_MAX_BYTES + 1).to_be_bytes())
        .await
        .expect("write a length");
    liar.write_all(&[1u8]).await.expect("write a kind");
    // A refusal comes back before the hangup, and that is deliberate: a client
    // that got the framing wrong must be able to tell that from a socket nobody
    // was listening on. It is the LAST thing this connection gets.
    let mut budget = Budget::new(
        CONNECTION_INGRESS_BYTES_PER_WINDOW,
        CONNECTION_EGRESS_BYTES_PER_WINDOW,
    );
    match read_frame(&mut liar, FRAME_BODY_MAX_BYTES, &mut budget)
        .await
        .expect("read the refusal")
    {
        Incoming::Frame(frame) => {
            match serde_json::from_slice(&frame.body).expect("decode the refusal") {
                ServerControl::HelloRefusal { code, .. } => {
                    assert_eq!(code, KernelErrorCode::FrameSize)
                }
                other => panic!("{other:?}"),
            }
        }
        Incoming::Closed => panic!("the daemon hung up without saying why"),
    }
    match read_frame(&mut liar, FRAME_BODY_MAX_BYTES, &mut budget).await {
        Ok(Incoming::Closed) => {}
        // A reset rather than a clean EOF, and correctly so: the refusal was
        // decided from the four length bytes, so the kind byte after them was
        // never consumed and the daemon closed with data still queued. Both
        // outcomes say the same thing — the connection is gone.
        Err(error) => assert!(
            error.fatal,
            "a non-fatal error on a dead connection: {error}"
        ),
        Ok(Incoming::Frame(frame)) => {
            panic!("the connection outlived the frame that broke it: {frame:?}")
        }
    }

    // A well-framed lie: through the handshake, then a known request carrying a
    // field the contract does not have. Strict decoding closes the connection
    // rather than ignoring the field, because a field that was ignored is a
    // client believing something it was never told.
    let (mut sneak, _) = Client::connect(&served.path).await;
    sneak
        .send(r#"{"type":"request","request_id":"r-x","request":{"type":"health","extra":1}}"#)
        .await;
    assert!(
        sneak.recv().await.is_none(),
        "an unknown field was tolerated"
    );

    // Both liars are gone; the bystander never noticed either of them.
    assert!(matches!(
        bystander.ask("r-after", r#"{"type":"health"}"#).await,
        KernelResult::Health { .. }
    ));

    drop(bystander);
    drop(sneak);
    served.close().await;
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_stream_survives_losing_the_listener_it_was_being_notified_through() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wire_deaf", 8).await;
    let admin = connect_pool(&secret(&name), 2).await.expect("connect");
    let served = Running::open(store, "deaf").await;

    let mut watcher = served.client().await;
    let watermark = watermark_of(&mut watcher, "r-wm").await;
    match watcher.ask("r-sub", &subscribe_from(watermark)).await {
        KernelResult::Subscribed { .. } => {}
        other => panic!("{other:?}"),
    }

    // A committed append that notifies NOBODY, under a listener that is perfectly
    // healthy. This is the case the poll exists for and the only way to produce
    // it deterministically — the append path queues its `pg_notify` inside the
    // transaction, so nothing that goes through the kernel can lose one on
    // purpose. `Running` started the listener, so it is not absent; it simply was
    // never told, exactly as it would not be after a dropped connection.
    let first = seed_events(&admin, 3).await;
    let batch = tokio::time::timeout(
        std::time::Duration::from_secs(SUBSCRIPTION_POLL_SECS + 5),
        watcher.recv(),
    )
    .await
    .expect("only the poll could have delivered this, and it had to")
    .expect("the connection stayed open");
    match batch {
        ServerControl::EventBatch { events, cursor, .. } => {
            assert!(events.iter().all(|e| e.global_sequence > watermark));
            assert_eq!(cursor, first, "the batch did not reach the seeded tail");
        }
        other => panic!("{other:?}"),
    }

    // Now take the channel away entirely, mid-stream: `Running`'s listener is a
    // live backend and it goes away under a subscription that is using it. What
    // PostgreSQL will not do is redeliver anything sent while it was gone.
    let killed: Vec<bool> = sqlx::query_scalar(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = current_database() AND pid <> pg_backend_pid() \
           AND query LIKE 'LISTEN %'",
    )
    .fetch_all(&admin)
    .await
    .expect("terminate the listener");
    assert!(
        killed.iter().any(|ok| *ok),
        "no LISTEN backend was found to kill: this half proved nothing"
    );

    // Still bounded, and still without a notification. The stream did not merely
    // survive one gap and then stall on a dead channel.
    let second = seed_events(&admin, 3).await;
    let batch = tokio::time::timeout(
        std::time::Duration::from_secs(SUBSCRIPTION_POLL_SECS + 5),
        watcher.recv(),
    )
    .await
    .expect("a stream whose listener died still delivers")
    .expect("the connection stayed open");
    match batch {
        ServerControl::EventBatch { cursor, .. } => {
            assert_eq!(cursor, second, "the stream stalled at the break")
        }
        other => panic!("{other:?}"),
    }

    drop(watcher);
    served.close().await;
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL, and waits out SLOW_CONSUMER_TIMEOUT_SECS on purpose"]
async fn a_consumer_that_stops_reading_is_cut_off_at_the_last_batch_it_actually_got() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wire_slow", 8).await;

    // Enough batches that the queue fills and STAYS full: one per subscription
    // the connection may hold, plus the one the writer is holding, plus the one
    // whose send has to block for the timeout to be reached — and a few over, so
    // this does not sit exactly on the boundary. 256 is the catch-up page.
    let before = store
        .watermark()
        .await
        .expect("watermark")
        .expect("genesis");
    let batches_needed = MAX_SUBSCRIPTIONS_PER_CONNECTION as u64 + 6;
    seed_events(store.pool(), batches_needed * 256).await;
    let (daemon, blobs) = daemon_for(store, "slow").await;
    let dir = runtime_dir("slow");

    // A pipe, not a socket, and small: the writer must be blocked mid-frame when
    // the timeout fires, which is the whole state under test. Over a socket that
    // depends on `net.core.wmem_default`.
    let (mine, theirs) = tokio::io::duplex(4096);
    let serving = tokio::spawn(async move {
        let (mut reader, mut writer) = tokio::io::split(theirs);
        let _ = gwk_kernel::wire::serve::serve_connection(&daemon, &mut reader, &mut writer).await;
    });
    let (mut client, _) = Client::greet(mine).await;

    match client.ask("r-sub", &subscribe_from(before)).await {
        KernelResult::Subscribed { .. } => {}
        other => panic!("{other:?}"),
    }

    // Read two batches and then stop, which is what makes this a SLOW consumer
    // rather than an absent one: some of the stream arrived, and the cursor it is
    // sent home with has to be inside that part.
    let mut received: Vec<Seq> = Vec::new();
    for _ in 0..2 {
        match client.recv().await.expect("the connection stayed open") {
            ServerControl::EventBatch { cursor, .. } => received.push(cursor),
            other => panic!("{other:?}"),
        }
    }

    // Nothing is read for longer than the kernel is willing to hold a batch.
    tokio::time::sleep(std::time::Duration::from_secs(
        SLOW_CONSUMER_TIMEOUT_SECS + 3,
    ))
    .await;

    // Now drain. `StreamClosed` travels on the response queue, so it overtakes
    // every batch still sitting in the batch queue — but not the one the writer
    // was already mid-frame on, which arrives first.
    let closed = loop {
        match client.recv().await.expect("the connection stayed open") {
            ServerControl::EventBatch { cursor, .. } => received.push(cursor),
            ServerControl::StreamClosed {
                request_id,
                code,
                last_cursor,
            } => {
                assert_eq!(request_id.as_str(), "r-sub");
                assert_eq!(code, KernelErrorCode::SlowConsumer);
                break last_cursor.expect("the consumer had received batches");
            }
            other => panic!("{other:?}"),
        }
    };

    // The claim: the cursor is one the client HELD IN ITS HAND, not one the
    // kernel had merely read to. Before this was measured at the write, the
    // reported cursor was the read position — every batch the queue was holding
    // plus the one in flight past the last frame that left, so a client resuming
    // from it would have skipped thousands of events it never saw.
    let position = received
        .iter()
        .position(|cursor| *cursor == closed)
        .unwrap_or_else(|| {
            panic!("the stream closed at {closed:?}, which was never sent: {received:?}")
        });
    // The last frame written, or the one before it: the frame the writer is
    // mid-way through when the timeout fires is not delivered yet, so trailing by
    // one is the documented behaviour. Trailing repeats an event on resume, which
    // at-least-once allows; leading loses one, which is the bug.
    assert!(
        received.len() - 1 - position <= 1,
        "closed at {closed:?}, {} batches behind the {} that were sent",
        received.len() - 1 - position,
        received.len()
    );
    assert!(
        closed.value() < before.value() + batches_needed * 256,
        "the kernel reported its own read position, not what it delivered"
    );

    drop(client);
    serving.await.expect("join");
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&blobs);
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

/// Begin an upload of `plaintext` and return the id the store minted.
async fn begin_upload(client: &mut Client, id: &str, plaintext: &[u8]) -> String {
    match client
        .ask(
            id,
            &format!(
                r#"{{"type":"blob_begin","media_type":"application/octet-stream","byte_size":"{}"}}"#,
                plaintext.len()
            ),
        )
        .await
    {
        KernelResult::BlobBegun { upload_id } => upload_id.as_str().to_owned(),
        other => panic!("{other:?}"),
    }
}

/// One `blob_chunk` request, base64 as the wire carries it.
fn chunk_request(upload_id: &str, sequence: u32, chunk: &[u8]) -> String {
    format!(
        r#"{{"type":"blob_chunk","upload_id":"{upload_id}","sequence":{sequence},"data_base64":"{}"}}"#,
        BASE64_STANDARD.encode(chunk)
    )
}

/// Read a blob whole over the wire, one clamped range at a time.
async fn read_blob(client: &mut Client, address: &str, size: usize) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(size);
    while out.len() < size {
        let request = format!(
            r#"{{"type":"blob_read","address":"{address}","offset":"{}","length":"{}"}}"#,
            out.len(),
            size - out.len()
        );
        match client.ask(&format!("r-read{}", out.len()), &request).await {
            KernelResult::BlobBytes {
                offset,
                data_base64,
                ..
            } => {
                // The echoed offset is what this range began at, so a client
                // never has to trust its own arithmetic.
                assert_eq!(offset.value(), out.len() as u64);
                let part = BASE64_STANDARD.decode(&data_base64).expect("base64");
                assert!(!part.is_empty(), "the read stalled at {}", out.len());
                out.extend_from_slice(&part);
            }
            other => panic!("{other:?}"),
        }
    }
    out
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_blob_goes_up_in_chunks_and_comes_back_byte_for_byte() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wire_blob", 8).await;
    let mut served = Served::open(store, "blob").await;

    // Not a round number, and not compressible into a pattern a wrong offset
    // would still satisfy.
    let plaintext: Vec<u8> = (0..9_001u32).map(|i| (i % 251) as u8).collect();
    let address = address_of(&plaintext);

    let upload = begin_upload(&mut served.client, "r-begin", &plaintext).await;
    // Deliberately uneven chunks: `commit` re-chunks the staged plaintext into
    // the container itself, so how a client splits its upload is its own
    // business and not the format's.
    for (sequence, chunk) in [&plaintext[..1], &plaintext[1..5_000], &plaintext[5_000..]]
        .into_iter()
        .enumerate()
    {
        let sequence = sequence as u32;
        match served
            .client
            .ask(
                &format!("r-chunk{sequence}"),
                &chunk_request(&upload, sequence, chunk),
            )
            .await
        {
            KernelResult::BlobChunkAccepted {
                upload_id,
                sequence: acked,
            } => {
                assert_eq!(upload_id.as_str(), upload);
                // Acknowledged by sequence, so a client can tell which chunk it
                // is being told about.
                assert_eq!(acked, sequence);
            }
            other => panic!("chunk {sequence}: {other:?}"),
        }
    }

    let descriptor = match served
        .client
        .ask(
            "r-commit",
            &format!(
                r#"{{"type":"blob_commit","upload_id":"{upload}","address":"{}"}}"#,
                address.as_str()
            ),
        )
        .await
    {
        KernelResult::BlobCommitted {
            descriptor,
            deduplicated,
        } => {
            assert!(!deduplicated, "nothing was there to deduplicate against");
            assert_eq!(descriptor.address, address);
            assert_eq!(descriptor.byte_size.value(), plaintext.len() as u64);
            assert!(!descriptor.tombstoned);
            descriptor
        }
        other => panic!("{other:?}"),
    };

    match served
        .client
        .ask(
            "r-stat",
            &format!(r#"{{"type":"blob_stat","address":"{}"}}"#, address.as_str()),
        )
        .await
    {
        // The same descriptor the commit reported: a stat is a re-read, not a
        // second opinion.
        KernelResult::BlobStat { descriptor: stat } => assert_eq!(stat, descriptor),
        other => panic!("{other:?}"),
    }

    // Byte for byte through encryption, chunking, and base64 both ways. Nothing
    // short of the real bytes passes this.
    let back = read_blob(&mut served.client, address.as_str(), plaintext.len()).await;
    assert_eq!(back, plaintext, "the blob did not come back as it went in");

    served.close().await;
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn the_same_bytes_uploaded_twice_are_stored_once() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wire_blob_dedup", 8).await;
    let mut served = Served::open(store, "blobdedup").await;

    let plaintext = b"the same bytes, twice".to_vec();
    let address = address_of(&plaintext);
    let commit = format!(
        r#"{{"type":"blob_commit","upload_id":"{{upload}}","address":"{}"}}"#,
        address.as_str()
    );

    for round in 0..2 {
        let upload = begin_upload(&mut served.client, &format!("r-begin{round}"), &plaintext).await;
        match served
            .client
            .ask(
                &format!("r-chunk{round}"),
                &chunk_request(&upload, 0, &plaintext),
            )
            .await
        {
            KernelResult::BlobChunkAccepted { .. } => {}
            other => panic!("{other:?}"),
        }
        match served
            .client
            .ask(
                &format!("r-commit{round}"),
                &commit.replace("{upload}", &upload),
            )
            .await
        {
            KernelResult::BlobCommitted { deduplicated, .. } => assert_eq!(
                deduplicated,
                round == 1,
                "round {round} reported the wrong dedup verdict"
            ),
            other => panic!("{other:?}"),
        }
    }

    served.close().await;
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn an_upload_that_does_not_add_up_is_refused_and_can_be_abandoned() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wire_blob_bad", 8).await;
    let mut served = Served::open(store, "blobbad").await;

    let plaintext = b"eleven bytes".to_vec();
    let upload = begin_upload(&mut served.client, "r-begin", &plaintext).await;

    // Not base64. Refused as the CLIENT's mistake — the upload is untouched and
    // still expects chunk 0.
    match served
        .client
        .ask(
            "r-garbage",
            &format!(
                r#"{{"type":"blob_chunk","upload_id":"{upload}","sequence":0,"data_base64":"not base64 at all!"}}"#
            ),
        )
        .await
    {
        KernelResult::Error { code, .. } => assert_eq!(code, KernelErrorCode::Validation),
        other => panic!("{other:?}"),
    }

    // Out of order. Refused as an INTEGRITY failure rather than reordered: the
    // staged file is a stream and a gap in it cannot be filled in later.
    match served
        .client
        .ask("r-gap", &chunk_request(&upload, 3, &plaintext))
        .await
    {
        KernelResult::Error { code, .. } => assert_eq!(code, KernelErrorCode::BlobIntegrity),
        other => panic!("{other:?}"),
    }

    // A chunk over the contract's size, refused before it is staged.
    match served
        .client
        .ask(
            "r-oversized",
            &chunk_request(&upload, 0, &vec![0u8; BLOB_CHUNK_BYTES + 1]),
        )
        .await
    {
        KernelResult::Error { code, message, .. } => {
            assert_eq!(code, KernelErrorCode::Validation);
            assert!(message.contains("chunk"), "{message}");
        }
        other => panic!("{other:?}"),
    }

    // The right bytes, then a commit claiming somebody else's digest.
    match served
        .client
        .ask("r-chunk", &chunk_request(&upload, 0, &plaintext))
        .await
    {
        KernelResult::BlobChunkAccepted { .. } => {}
        other => panic!("{other:?}"),
    }
    let wrong = address_of(b"different bytes entirely");
    match served
        .client
        .ask(
            "r-liar",
            &format!(
                r#"{{"type":"blob_commit","upload_id":"{upload}","address":"{}"}}"#,
                wrong.as_str()
            ),
        )
        .await
    {
        // The kernel hashes what it staged; the claim is checked, not trusted.
        KernelResult::Error { code, .. } => assert_eq!(code, KernelErrorCode::BlobIntegrity),
        other => panic!("{other:?}"),
    }

    match served
        .client
        .ask(
            "r-abort",
            &format!(r#"{{"type":"blob_abort","upload_id":"{upload}"}}"#),
        )
        .await
    {
        KernelResult::BlobAborted { upload_id } => assert_eq!(upload_id.as_str(), upload),
        other => panic!("{other:?}"),
    }
    // And it is gone: a second chunk has nowhere to land.
    match served
        .client
        .ask("r-after-abort", &chunk_request(&upload, 1, &plaintext))
        .await
    {
        KernelResult::Error { code, .. } => assert_eq!(code, KernelErrorCode::NotFound),
        other => panic!("{other:?}"),
    }

    served.close().await;
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_read_larger_than_a_frame_is_clamped_rather_than_refused() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wire_blob_clamp", 8).await;
    let mut served = Served::open(store, "blobclamp").await;

    let plaintext = b"a few bytes".to_vec();
    let address = address_of(&plaintext);
    let upload = begin_upload(&mut served.client, "r-begin", &plaintext).await;
    match served
        .client
        .ask("r-chunk", &chunk_request(&upload, 0, &plaintext))
        .await
    {
        KernelResult::BlobChunkAccepted { .. } => {}
        other => panic!("{other:?}"),
    }
    match served
        .client
        .ask(
            "r-commit",
            &format!(
                r#"{{"type":"blob_commit","upload_id":"{upload}","address":"{}"}}"#,
                address.as_str()
            ),
        )
        .await
    {
        KernelResult::BlobCommitted { .. } => {}
        other => panic!("{other:?}"),
    }

    // A length no frame could ever carry. Clamped, so the answer is bytes and an
    // offset rather than a refusal the client has to special-case — the same
    // rule the log and the projections follow.
    match served
        .client
        .ask(
            "r-huge",
            &format!(
                r#"{{"type":"blob_read","address":"{}","offset":"0","length":"{}"}}"#,
                address.as_str(),
                u64::from(u32::MAX)
            ),
        )
        .await
    {
        KernelResult::BlobBytes { data_base64, .. } => {
            let bytes = BASE64_STANDARD.decode(&data_base64).expect("base64");
            assert_eq!(bytes, plaintext);
        }
        other => panic!("{other:?}"),
    }

    served.close().await;
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_blob_that_was_never_written_is_absent_rather_than_an_error() {
    let maintenance = maintenance_pool().await;
    // Sealed on purpose. Sealing gates COMMANDS; the blob spine is open before
    // cutover, which is what lets an operator stage evidence at all.
    let (name, store) = fresh_sealed_store(&maintenance, "wire_unwritten", 8).await;
    let dir = runtime_dir("unwritten");
    let path = dir.join("gwk.sock");
    let listener = Listener::bind(&path).await.expect("bind");
    let (daemon, blob_root) = daemon_for(store, "unwritten").await;
    let daemon = Arc::new(daemon);

    let serving = tokio::spawn({
        let daemon = Arc::clone(&daemon);
        async move {
            let (stream, _) = listener.accept().await.expect("accept");
            let _ = serve_stream(&daemon, stream).await;
            listener.remove();
        }
    });

    let (mut client, _) = Client::connect(&path).await;
    // A legal address nothing was ever written at.
    let address = format!("sha256:{}", "0".repeat(64));
    match client
        .ask(
            "r-stat",
            &format!(r#"{{"type":"blob_stat","address":"{address}"}}"#),
        )
        .await
    {
        // Absent, not tombstoned: the two are different facts and a retention
        // audit reads them differently.
        KernelResult::Error { code, .. } => assert_eq!(code, KernelErrorCode::NotFound),
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
    let _ = std::fs::remove_dir_all(&blob_root);
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
    let (daemon, blob_root) = daemon_for(store, "drain").await;
    let daemon = Arc::new(daemon);

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
    let _ = std::fs::remove_dir_all(&blob_root);
    drop_database(&maintenance, &name).await;
}

// ---- The PTY surface: a host publishes, a viewer attaches ----

/// A blank all-false-style cell for building publishable frames.
fn pty_cell(glyph: &str) -> gwk_domain::frame::StyledCell {
    gwk_domain::frame::StyledCell {
        glyph: glyph.to_owned(),
        style: gwk_domain::frame::CellStyle {
            bold: false,
            dim: false,
            italic: false,
            blink: false,
            inverse: false,
            invisible: false,
            strikethrough: false,
            overline: false,
            underline: None,
            fg: None,
            bg: None,
            underline_color: None,
        },
    }
}

fn pty_frame(rows: usize, cols: usize) -> gwk_domain::frame::PtyFrame {
    gwk_domain::frame::PtyFrame::from_cells(&vec![vec![pty_cell(" "); cols]; rows], None)
}

async fn submit_kernel_command(
    client: &mut Client,
    request_id: &str,
    key: &str,
    actor_kind: &str,
    command: &gwk_domain::KernelCommand,
) -> KernelResult {
    let request = gwk_domain::KernelRequest::SubmitCommand {
        envelope: envelope_as(key, actor(actor_kind), command),
    };
    client
        .ask(
            request_id,
            &serde_json::to_string(&request).expect("serialize command request"),
        )
        .await
}

async fn send_pty_input(
    client: &mut Client,
    request_id: &str,
    key: &str,
    actor_kind: &str,
    session_id: &str,
    generation: &gwk_domain::PtySessionGeneration,
    bytes: &[u8],
) -> KernelResult {
    let command = gwk_domain::KernelCommand::SendPtyInput {
        pty_session_id: gwk_domain::PtySessionId::new(session_id),
        generation: generation.clone(),
        byte_count: gwk_domain::ByteCount::new(bytes.len() as u64),
    };
    // A host-published session is owned by the SYSTEM project — the kernel
    // authors its lifecycle receipts there — so a send addressing one has to
    // name that project or the cross-project ownership check refuses it as a
    // validation error before authority is ever evaluated.
    let request = gwk_domain::KernelRequest::SendPtyInput {
        envelope: envelope_in(gwk_kernel::SYSTEM_PROJECT, key, actor(actor_kind), &command),
        data_base64: PtyInputData::new(BASE64_STANDARD.encode(bytes)),
    };
    client
        .ask(
            request_id,
            &serde_json::to_string(&request).expect("serialize input request"),
        )
        .await
}

async fn submit_pty_control(
    client: &mut Client,
    request_id: &str,
    request: KernelRequest,
) -> KernelResult {
    client
        .ask(
            request_id,
            &serde_json::to_string(&request).expect("serialize PTY control request"),
        )
        .await
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn generic_submit_refuses_delivery_only_pty_commands() {
    use gwk_domain::{KernelCommand, PtySessionGeneration, PtySessionId, PtySessionTemplateName};

    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wire_pty_generic_refusal", 8).await;
    let served = Running::open(store, "ptygeneric").await;
    let mut client = served.client_with_capabilities(&[]).await;
    let commands = [
        KernelCommand::ResizePtySession {
            pty_session_id: PtySessionId::new("console"),
            generation: PtySessionGeneration::new("life-1"),
            cols: 120,
            rows: 40,
        },
        KernelCommand::StopPtySession {
            pty_session_id: PtySessionId::new("console"),
            generation: PtySessionGeneration::new("life-1"),
        },
        KernelCommand::RequestPtySessionStart {
            template_name: PtySessionTemplateName::new("review"),
            pty_session_id: PtySessionId::new("review-1"),
        },
    ];

    for (index, command) in commands.iter().enumerate() {
        let request = format!("generic-{index}");
        let result =
            submit_kernel_command(&mut client, &request, &request, "operator", command).await;
        let KernelResult::Error { code, message, .. } = result else {
            panic!("generic submit unexpectedly accepted a delivery-only command: {result:?}");
        };
        assert_eq!(code, KernelErrorCode::Validation);
        assert!(message.contains("dedicated PTY request"), "{message}");
    }

    drop(client);
    served.close().await;
    drop_database(&maintenance, &name).await;
}

/// The sweep's idempotency namespace is not reachable from the wire.
///
/// A stale attempt's version does not advance while it waits, so the sweep
/// recomputes the identical key every tick — a client command landed on that
/// key first would make the kernel's own burial refuse identically forever,
/// silently disabling the sweep for that row. The reject arm proves the
/// namespace is closed; the accept arm proves the gate is the prefix, not
/// the command.
#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn the_sweeps_idempotency_namespace_is_refused_at_the_wire() {
    use gwk_domain::TaskId;

    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wire_reserved_key", 8).await;
    let served = Running::open(store, "reservedkey").await;
    let mut client = served.client_with_capabilities(&[]).await;

    let command = KernelCommand::CreateTask {
        task_id: TaskId::new("t-reserved"),
        kind: None,
        title: None,
        spec_ref: None,
        project: None,
        priority: None,
        tracker_ref: None,
    };

    let result = submit_kernel_command(
        &mut client,
        "reserved-1",
        "ttl_sweep:attempt_stale:a-victim:4",
        "operator",
        &command,
    )
    .await;
    let KernelResult::Error { code, message, .. } = result else {
        panic!("a ttl_sweep:-prefixed key was accepted: {result:?}");
    };
    assert_eq!(code, KernelErrorCode::Validation);
    assert!(message.contains("reserved"), "{message}");

    let result = submit_kernel_command(
        &mut client,
        "reserved-2",
        "client-key-1",
        "operator",
        &command,
    )
    .await;
    assert!(
        matches!(result, KernelResult::CommandApplied { .. }),
        "an ordinary key stopped working: {result:?}"
    );

    drop(client);
    served.close().await;
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn dedicated_pty_requests_validate_their_command_before_commit() {
    use gwk_domain::{KernelCommand, PtySessionGeneration, PtySessionId};

    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wire_pty_cross_verb", 8).await;
    let pool = store.pool().clone();
    let served = Running::open(store, "ptycrossverb").await;
    let mut client = served
        .client_with_capabilities(&[PTY_CONTROL_CAPABILITY])
        .await;
    let stop = KernelCommand::StopPtySession {
        pty_session_id: PtySessionId::new("console"),
        generation: PtySessionGeneration::new("life-1"),
    };
    let envelope = envelope_in(
        gwk_kernel::SYSTEM_PROJECT,
        "resize-carrying-stop",
        actor("operator"),
        &stop,
    );
    let request = KernelRequest::ResizePtySession { envelope };

    let result = submit_pty_control(&mut client, "resize-carrying-stop", request).await;
    assert!(matches!(
        result,
        KernelResult::Error {
            code: KernelErrorCode::Validation,
            ..
        }
    ));
    let durable: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gwk.event WHERE idempotency_key = 'resize-carrying-stop'",
    )
    .fetch_one(&pool)
    .await
    .expect("cross-verb event count");
    assert_eq!(durable, 0, "a mismatched dedicated verb must not commit");

    drop(client);
    served.close().await;
    drop(pool);
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn receipted_resize_stop_and_declared_start_route_across_the_real_wire() {
    use gwk_domain::{KernelCommand, PtySessionTemplateName};
    use sqlx::Row;

    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wire_pty_controls", 8).await;
    let pool = store.pool().clone();
    let served = Running::open(store, "ptycontrols").await;
    let mut host = served
        .client_with_capabilities(&[PTY_INPUT_CAPABILITY, PTY_CONTROL_CAPABILITY])
        .await;
    let mut manager = served
        .client_with_capabilities(&[PTY_START_CAPABILITY])
        .await;
    let mut sender = served
        .client_with_capabilities(&[PTY_CONTROL_CAPABILITY])
        .await;
    let frame = serde_json::to_string(&pty_frame(2, 4)).expect("serialize frame");
    assert!(matches!(
        host.ask(
            "seed",
            &format!(
                r#"{{"type":"pty_publish_snapshot","session_id":"console","seq":"0","frame":{frame}}}"#
            ),
        )
        .await,
        KernelResult::PtyPublished { .. }
    ));
    let generation = match sender
        .ask(
            "generation",
            r#"{"type":"pty_snapshot","session_id":"console"}"#,
        )
        .await
    {
        KernelResult::PtySnapshot { generation, .. } => generation,
        other => panic!("generation: {other:?}"),
    };

    let resize = KernelCommand::ResizePtySession {
        pty_session_id: gwk_domain::PtySessionId::new("console"),
        generation: generation.clone(),
        cols: 120,
        rows: 40,
    };
    let resize_request = KernelRequest::ResizePtySession {
        envelope: envelope_in(
            gwk_kernel::SYSTEM_PROJECT,
            "resize",
            actor("operator"),
            &resize,
        ),
    };
    let resize_send = submit_pty_control(&mut sender, "resize-request", resize_request);
    let resize_apply = async {
        let control = tokio::time::timeout(std::time::Duration::from_secs(3), host.recv())
            .await
            .expect("resize reaches host")
            .expect("host remains open");
        let ServerControl::PtyResize {
            delivery_id,
            cols: 120,
            rows: 40,
            ..
        } = control
        else {
            panic!("unexpected resize delivery: {control:?}");
        };
        let row_lock_released =
            tokio::time::timeout(std::time::Duration::from_millis(500), async {
                let mut transaction = pool.begin().await.expect("begin lock probe");
                sqlx::query(
                    "SELECT 1 FROM gwk_internal.pty_delivery \
                     WHERE idempotency_key = 'resize' FOR UPDATE",
                )
                .execute(&mut *transaction)
                .await
                .expect("lock pending delivery");
                transaction.rollback().await.expect("release lock probe");
            })
            .await
            .is_ok();
        host.send(
            &serde_json::to_string(&gwk_domain::ClientControl::PtyDeliveryAck {
                delivery_id,
                result: gwk_domain::protocol::PtyDeliveryResult::Applied,
            })
            .expect("serialize resize ack"),
        )
        .await;
        assert!(
            row_lock_released,
            "host application must not hold a database row lock or pool connection"
        );
    };
    let (resize_result, ()) = tokio::join!(resize_send, resize_apply);
    assert!(matches!(resize_result, KernelResult::CommandApplied { .. }));

    let template = KernelCommand::DeclarePtySessionTemplate {
        template_name: PtySessionTemplateName::new("review"),
        command: "/bin/cat".to_owned(),
        args: vec![],
        cwd: None,
        env: std::collections::BTreeMap::new(),
        cols: 100,
        rows: 30,
    };
    assert!(matches!(
        submit_kernel_command(
            &mut sender,
            "declare-request",
            "declare",
            "operator",
            &template,
        )
        .await,
        KernelResult::CommandApplied { .. }
    ));
    let start = KernelCommand::RequestPtySessionStart {
        template_name: PtySessionTemplateName::new("review"),
        pty_session_id: gwk_domain::PtySessionId::new("review-7"),
    };
    let start_send = submit_pty_control(
        &mut sender,
        "start-request",
        KernelRequest::StartPtySession {
            envelope: envelope_in(
                gwk_kernel::SYSTEM_PROJECT,
                "start",
                actor("operator"),
                &start,
            ),
        },
    );
    let start_apply = async {
        let control = tokio::time::timeout(std::time::Duration::from_secs(3), manager.recv())
            .await
            .expect("start reaches manager")
            .expect("manager remains open");
        let delivery_id = match control {
            ServerControl::PtyStart {
                delivery_id,
                template_name,
                session_id,
                ..
            } => {
                assert_eq!(template_name.as_str(), "review");
                assert_eq!(session_id.as_str(), "review-7");
                delivery_id
            }
            other => panic!("manager received {other:?}"),
        };
        manager
            .send(
                &serde_json::to_string(&gwk_domain::ClientControl::PtyDeliveryAck {
                    delivery_id,
                    result: gwk_domain::protocol::PtyDeliveryResult::Applied,
                })
                .expect("serialize start ack"),
            )
            .await;
    };
    let (start_result, ()) = tokio::join!(start_send, start_apply);
    assert!(matches!(start_result, KernelResult::CommandApplied { .. }));

    let stop = KernelCommand::StopPtySession {
        pty_session_id: gwk_domain::PtySessionId::new("console"),
        generation: generation.clone(),
    };
    let stop_send = submit_pty_control(
        &mut sender,
        "stop-request",
        KernelRequest::StopPtySession {
            envelope: envelope_in(gwk_kernel::SYSTEM_PROJECT, "stop", actor("operator"), &stop),
        },
    );
    let stop_apply = async {
        let control = tokio::time::timeout(std::time::Duration::from_secs(3), host.recv())
            .await
            .expect("stop reaches host")
            .expect("host remains open");
        let ServerControl::PtyStop { delivery_id, .. } = control else {
            panic!("unexpected stop delivery: {control:?}");
        };
        host.send(
            &serde_json::to_string(&gwk_domain::ClientControl::PtyDeliveryAck {
                delivery_id,
                result: gwk_domain::protocol::PtyDeliveryResult::Applied,
            })
            .expect("serialize stop ack"),
        )
        .await;
    };
    let (stop_result, ()) = tokio::join!(stop_send, stop_apply);
    assert!(matches!(stop_result, KernelResult::CommandApplied { .. }));

    let receipts = sqlx::query(
        "SELECT action, observed_basis FROM gwk.receipt \
         WHERE id IN ('receipt:system:resize', 'receipt:system:start', 'receipt:system:stop') \
         ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .expect("control receipts");
    assert_eq!(receipts.len(), 3);
    assert!(receipts.iter().any(|row| {
        row.get::<String, _>("action") == "pty_control"
            && row
                .get::<String, _>("observed_basis")
                .contains("cols=120; rows=40")
    }));
    assert!(receipts.iter().any(|row| {
        row.get::<String, _>("action") == "pty_control"
            && row.get::<String, _>("observed_basis").contains("stop=true")
    }));
    assert!(receipts.iter().any(|row| {
        row.get::<String, _>("action") == "pty_start"
            && row
                .get::<String, _>("observed_basis")
                .contains("template=review")
            && !row.get::<String, _>("observed_basis").contains("/bin/cat")
    }));

    drop(host);
    drop(manager);
    drop(sender);
    served.close().await;
    drop(pool);
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn host_refusal_is_terminal_without_reconnect_and_stop_still_reaches_the_child() {
    use gwk_domain::KernelCommand;
    use sqlx::Row;

    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wire_pty_apply_ack", 8).await;
    let pool = store.pool().clone();
    let served = Running::open(store, "ptyapplyack").await;
    let mut host = served
        .client_with_capabilities(&[PTY_CONTROL_CAPABILITY, PTY_INPUT_CAPABILITY])
        .await;
    let mut sender = served
        .client_with_capabilities(&[PTY_CONTROL_CAPABILITY, PTY_INPUT_CAPABILITY])
        .await;
    let frame = serde_json::to_string(&pty_frame(2, 4)).expect("serialize frame");
    assert!(matches!(
        host.ask(
            "seed",
            &format!(
                r#"{{"type":"pty_publish_snapshot","session_id":"console","seq":"0","frame":{frame}}}"#
            ),
        )
        .await,
        KernelResult::PtyPublished { .. }
    ));
    let generation = match sender
        .ask(
            "generation",
            r#"{"type":"pty_snapshot","session_id":"console"}"#,
        )
        .await
    {
        KernelResult::PtySnapshot { generation, .. } => generation,
        other => panic!("generation: {other:?}"),
    };

    let resize = KernelCommand::ResizePtySession {
        pty_session_id: gwk_domain::PtySessionId::new("console"),
        generation: generation.clone(),
        cols: 120,
        rows: 40,
    };
    let resize_request = KernelRequest::ResizePtySession {
        envelope: envelope_in(
            gwk_kernel::SYSTEM_PROJECT,
            "resize-refused",
            actor("operator"),
            &resize,
        ),
    };
    let resize_send = submit_pty_control(&mut sender, "resize-refused", resize_request.clone());
    let resize_refuse = async {
        let control = host.recv().await.expect("resize delivery");
        let ServerControl::PtyResize { delivery_id, .. } = control else {
            panic!("unexpected resize: {control:?}");
        };
        host.send(
            &serde_json::to_string(&gwk_domain::ClientControl::PtyDeliveryAck {
                delivery_id,
                result: gwk_domain::protocol::PtyDeliveryResult::Refused {
                    code: KernelErrorCode::Validation,
                    message: "local resize refused".to_owned(),
                },
            })
            .expect("serialize refusal"),
        )
        .await;
    };
    let (resize_result, ()) = tokio::join!(resize_send, resize_refuse);
    assert!(matches!(resize_result, KernelResult::Error { .. }));
    let failed: bool = sqlx::query_scalar(
        "SELECT failed_at IS NOT NULL FROM gwk_internal.pty_delivery \
         WHERE idempotency_key = 'resize-refused'",
    )
    .fetch_one(&pool)
    .await
    .expect("failed delivery");
    assert!(failed, "host refusal must terminally settle delivery");

    let replay = submit_pty_control(&mut sender, "resize-replay", resize_request).await;
    assert!(matches!(replay, KernelResult::Error { .. }));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), host.recv())
            .await
            .is_err(),
        "a terminally refused replay must not redeliver"
    );

    let stop = KernelCommand::StopPtySession {
        pty_session_id: gwk_domain::PtySessionId::new("console"),
        generation: generation.clone(),
    };
    let stop_request = KernelRequest::StopPtySession {
        envelope: envelope_in(
            gwk_kernel::SYSTEM_PROJECT,
            "stop-fence",
            actor("operator"),
            &stop,
        ),
    };
    let stop_send = submit_pty_control(&mut sender, "stop-fence", stop_request);
    let stop_apply = async {
        let control = tokio::time::timeout(std::time::Duration::from_secs(3), host.recv())
            .await
            .expect("stop reaches the same host connection after refusal")
            .expect("host remains connected");
        let ServerControl::PtyStop { delivery_id, .. } = control else {
            panic!("unexpected stop delivery: {control:?}");
        };
        host.send(
            &serde_json::to_string(&gwk_domain::ClientControl::PtyDeliveryAck {
                delivery_id,
                result: gwk_domain::protocol::PtyDeliveryResult::Applied,
            })
            .expect("serialize stop acknowledgement"),
        )
        .await;
        assert!(matches!(
            host.ask(
                "retire-after-stop",
                r#"{"type":"pty_retire","session_id":"console"}"#,
            )
            .await,
            KernelResult::PtyRetired { .. }
        ));
    };
    let (stop_result, ()) = tokio::join!(stop_send, stop_apply);
    assert!(matches!(stop_result, KernelResult::CommandApplied { .. }));
    let row = sqlx::query("SELECT state FROM gwk.pty_session WHERE id = $1")
        .bind(format!("console:{generation}"))
        .fetch_one(&pool)
        .await
        .expect("session row");
    assert_eq!(row.get::<String, _>("state"), "closed");

    let later = send_pty_input(
        &mut sender,
        "input-after-stop",
        "input-after-stop",
        "operator",
        "console",
        &generation,
        b"x",
    )
    .await;
    assert!(matches!(
        later,
        KernelResult::Error {
            code: KernelErrorCode::StaleVersion,
            ..
        }
    ));

    drop(host);
    drop(sender);
    served.close().await;
    drop(pool);
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn oversized_host_refusal_prose_is_not_persisted() {
    use gwk_domain::KernelCommand;
    use sqlx::Row;

    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wire_pty_bounded_ack", 8).await;
    let pool = store.pool().clone();
    let served = Running::open(store, "ptyboundedack").await;
    let mut host = served
        .client_with_capabilities(&[PTY_CONTROL_CAPABILITY])
        .await;
    let mut sender = served
        .client_with_capabilities(&[PTY_CONTROL_CAPABILITY])
        .await;
    let frame = serde_json::to_string(&pty_frame(2, 4)).expect("serialize frame");
    assert!(matches!(
        host.ask(
            "seed",
            &format!(
                r#"{{"type":"pty_publish_snapshot","session_id":"console","seq":"0","frame":{frame}}}"#
            ),
        )
        .await,
        KernelResult::PtyPublished { .. }
    ));
    let generation = match sender
        .ask(
            "generation",
            r#"{"type":"pty_snapshot","session_id":"console"}"#,
        )
        .await
    {
        KernelResult::PtySnapshot { generation, .. } => generation,
        other => panic!("generation: {other:?}"),
    };
    let resize = KernelCommand::ResizePtySession {
        pty_session_id: gwk_domain::PtySessionId::new("console"),
        generation,
        cols: 120,
        rows: 40,
    };
    let request = KernelRequest::ResizePtySession {
        envelope: envelope_in(
            gwk_kernel::SYSTEM_PROJECT,
            "resize-oversized-refusal",
            actor("operator"),
            &resize,
        ),
    };

    let submitted = submit_pty_control(&mut sender, "resize-oversized-refusal", request);
    let refused = async {
        let control = host.recv().await.expect("resize delivery");
        let ServerControl::PtyResize { delivery_id, .. } = control else {
            panic!("unexpected resize: {control:?}");
        };
        let oversized = "x".repeat(4 * 1024 + 1);
        host.send(
            &serde_json::to_string(&gwk_domain::ClientControl::PtyDeliveryAck {
                delivery_id,
                result: gwk_domain::protocol::PtyDeliveryResult::Refused {
                    code: KernelErrorCode::Validation,
                    message: oversized,
                },
            })
            .expect("serialize oversized refusal"),
        )
        .await;
    };
    let (result, ()) = tokio::join!(submitted, refused);
    assert!(matches!(result, KernelResult::Error { .. }));

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let row = sqlx::query(
            "SELECT failed_at IS NOT NULL AS failed, \
                    indeterminate_at IS NOT NULL AS indeterminate, \
                    failure_code, failure_message FROM gwk_internal.pty_delivery \
             WHERE idempotency_key = 'resize-oversized-refusal'",
        )
        .fetch_one(&pool)
        .await
        .expect("delivery state");
        if row.get::<bool, _>("indeterminate") {
            assert!(!row.get::<bool, _>("failed"));
            assert!(row.get::<Option<String>, _>("failure_code").is_none());
            assert!(row.get::<Option<String>, _>("failure_message").is_none());
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "invalid acknowledgement must terminally settle without persisting host prose"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    drop(host);
    drop(sender);
    served.close().await;
    drop(pool);
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn host_disconnect_after_dispatch_settles_terminally_indeterminate() {
    use gwk_domain::KernelCommand;
    use sqlx::Row;

    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wire_pty_disconnect_ack", 8).await;
    let pool = store.pool().clone();
    let served = Running::open(store, "ptydisconnectack").await;
    let mut host = served
        .client_with_capabilities(&[PTY_CONTROL_CAPABILITY])
        .await;
    let mut sender = served
        .client_with_capabilities(&[PTY_CONTROL_CAPABILITY])
        .await;
    let frame = serde_json::to_string(&pty_frame(2, 4)).expect("serialize frame");
    assert!(matches!(
        host.ask(
            "seed",
            &format!(
                r#"{{"type":"pty_publish_snapshot","session_id":"console","seq":"0","frame":{frame}}}"#
            ),
        )
        .await,
        KernelResult::PtyPublished { .. }
    ));
    let generation = match sender
        .ask(
            "generation",
            r#"{"type":"pty_snapshot","session_id":"console"}"#,
        )
        .await
    {
        KernelResult::PtySnapshot { generation, .. } => generation,
        other => panic!("generation: {other:?}"),
    };
    let resize = KernelCommand::ResizePtySession {
        pty_session_id: gwk_domain::PtySessionId::new("console"),
        generation,
        cols: 120,
        rows: 40,
    };
    let request = KernelRequest::ResizePtySession {
        envelope: envelope_in(
            gwk_kernel::SYSTEM_PROJECT,
            "resize-disconnected",
            actor("operator"),
            &resize,
        ),
    };

    let submitted = submit_pty_control(&mut sender, "resize-disconnected", request);
    let disconnected = async {
        let control = tokio::time::timeout(std::time::Duration::from_secs(3), host.recv())
            .await
            .expect("resize reaches host")
            .expect("host remains open until dispatch");
        assert!(matches!(control, ServerControl::PtyResize { .. }));
        drop(host);
    };
    let (result, ()) = tokio::join!(submitted, disconnected);
    assert!(matches!(result, KernelResult::Error { .. }));

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let row = sqlx::query(
            "SELECT failed_at IS NOT NULL AS failed, \
                    indeterminate_at IS NOT NULL AS indeterminate \
             FROM gwk_internal.pty_delivery \
             WHERE idempotency_key = 'resize-disconnected'",
        )
        .fetch_one(&pool)
        .await
        .expect("delivery state");
        if row.get::<bool, _>("indeterminate") {
            assert!(!row.get::<bool, _>("failed"));
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "disconnect after dispatch must not leave a permanently pending delivery"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    let mut replacement = served
        .client_with_capabilities(&[PTY_CONTROL_CAPABILITY])
        .await;
    let replay = submit_pty_control(
        &mut sender,
        "resize-disconnected-replay",
        KernelRequest::ResizePtySession {
            envelope: envelope_in(
                gwk_kernel::SYSTEM_PROJECT,
                "resize-disconnected",
                actor("operator"),
                &resize,
            ),
        },
    )
    .await;
    assert!(matches!(
        replay,
        KernelResult::Error {
            code: KernelErrorCode::Indeterminate,
            ..
        }
    ));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), replacement.recv())
            .await
            .is_err(),
        "an indeterminate delivery must never be redelivered"
    );

    drop(replacement);
    drop(sender);
    served.close().await;
    drop(pool);
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn retained_applied_ack_reconciles_an_indeterminate_delivery() {
    use gwk_domain::KernelCommand;
    use sqlx::Row;

    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wire_pty_reconcile_ack", 8).await;
    let pool = store.pool().clone();
    let served = Running::open(store, "ptyreconcileack").await;
    let mut host = served
        .client_with_capabilities(&[PTY_CONTROL_CAPABILITY])
        .await;
    let mut sender = served
        .client_with_capabilities(&[PTY_CONTROL_CAPABILITY])
        .await;
    let frame = serde_json::to_string(&pty_frame(2, 4)).expect("serialize frame");
    assert!(matches!(
        host.ask(
            "seed",
            &format!(
                r#"{{"type":"pty_publish_snapshot","session_id":"console","seq":"0","frame":{frame}}}"#
            ),
        )
        .await,
        KernelResult::PtyPublished { .. }
    ));
    let generation = match sender
        .ask(
            "generation",
            r#"{"type":"pty_snapshot","session_id":"console"}"#,
        )
        .await
    {
        KernelResult::PtySnapshot { generation, .. } => generation,
        other => panic!("generation: {other:?}"),
    };
    let resize = KernelCommand::ResizePtySession {
        pty_session_id: gwk_domain::PtySessionId::new("console"),
        generation,
        cols: 120,
        rows: 40,
    };
    let request = KernelRequest::ResizePtySession {
        envelope: envelope_in(
            gwk_kernel::SYSTEM_PROJECT,
            "resize-reconciled",
            actor("operator"),
            &resize,
        ),
    };

    let submitted = submit_pty_control(&mut sender, "resize-reconciled", request.clone());
    let disconnected = async {
        let delivery_id = match tokio::time::timeout(std::time::Duration::from_secs(3), host.recv())
            .await
            .expect("resize reaches host")
            .expect("host remains open until dispatch")
        {
            ServerControl::PtyResize { delivery_id, .. } => delivery_id,
            other => panic!("unexpected control: {other:?}"),
        };
        drop(host);
        delivery_id
    };
    let (result, delivery_id) = tokio::join!(submitted, disconnected);
    assert!(matches!(result, KernelResult::Error { .. }));

    let mut reconnected = served
        .client_with_capabilities(&[PTY_CONTROL_CAPABILITY])
        .await;
    reconnected
        .send(
            &serde_json::to_string(&ClientControl::PtyDeliveryAck {
                delivery_id: delivery_id.clone(),
                result: PtyDeliveryResult::Applied,
            })
            .expect("serialize retained acknowledgement"),
        )
        .await;
    assert!(matches!(
        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            reconnected.recv_control(),
        )
            .await
            .expect("settlement confirmation arrives")
            .expect("reconnected host remains open"),
        ServerControl::PtyDeliverySettled { delivery_id: settled } if settled == delivery_id
    ));

    let row = sqlx::query(
        "SELECT delivered_at IS NOT NULL AS delivered, \
                indeterminate_at IS NOT NULL AS indeterminate \
         FROM gwk_internal.pty_delivery WHERE idempotency_key = 'resize-reconciled'",
    )
    .fetch_one(&pool)
    .await
    .expect("reconciled delivery state");
    assert!(row.get::<bool, _>("delivered"));
    assert!(!row.get::<bool, _>("indeterminate"));
    assert!(matches!(
        submit_pty_control(&mut sender, "resize-reconciled-replay", request).await,
        KernelResult::CommandApplied { .. }
    ));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), reconnected.recv())
            .await
            .is_err(),
        "a reconciled replay must not redeliver"
    );

    drop(reconnected);
    drop(sender);
    served.close().await;
    drop(pool);
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_failed_start_delivery_retries_and_a_later_lifetime_can_reuse_the_session_id() {
    use gwk_domain::{KernelCommand, PtySessionTemplateName};

    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wire_pty_start_retry", 8).await;
    let pool = store.pool().clone();
    let served = Running::open(store, "ptystartretry").await;
    let mut sender = served.client().await;
    let template = KernelCommand::DeclarePtySessionTemplate {
        template_name: PtySessionTemplateName::new("review"),
        command: "/bin/cat".to_owned(),
        args: vec![],
        cwd: None,
        env: std::collections::BTreeMap::new(),
        cols: 100,
        rows: 30,
    };
    assert!(matches!(
        submit_kernel_command(
            &mut sender,
            "declare-retry-template",
            "declare-retry-template",
            "operator",
            &template,
        )
        .await,
        KernelResult::CommandApplied { .. }
    ));

    let start = KernelCommand::RequestPtySessionStart {
        template_name: PtySessionTemplateName::new("review"),
        pty_session_id: gwk_domain::PtySessionId::new("reusable"),
    };
    let first_envelope = envelope_in(
        gwk_kernel::SYSTEM_PROJECT,
        "start-before-manager",
        actor("operator"),
        &start,
    );
    let first_request = KernelRequest::StartPtySession {
        envelope: first_envelope.clone(),
    };
    let first =
        submit_pty_control(&mut sender, "start-before-manager", first_request.clone()).await;
    let KernelResult::Error { detail, .. } = first else {
        panic!("a committed start with no manager must report delivery failure: {first:?}");
    };
    assert_eq!(
        detail
            .as_ref()
            .and_then(|detail| detail.get("command_committed"))
            .and_then(serde_json::Value::as_bool),
        Some(true),
    );

    let mut manager = served
        .client_with_capabilities(&[PTY_START_CAPABILITY])
        .await;
    let retry_send = submit_pty_control(&mut sender, "retry-after-manager", first_request);
    let retry_apply = async {
        let control = tokio::time::timeout(std::time::Duration::from_secs(3), manager.recv())
            .await
            .expect("the retry reaches the new manager")
            .expect("manager remains open");
        let ServerControl::PtyStart {
            delivery_id,
            session_id,
            ..
        } = control
        else {
            panic!("unexpected start retry: {control:?}");
        };
        assert_eq!(session_id.as_str(), "reusable");
        manager
            .send(
                &serde_json::to_string(&gwk_domain::ClientControl::PtyDeliveryAck {
                    delivery_id,
                    result: gwk_domain::protocol::PtyDeliveryResult::Applied,
                })
                .expect("serialize retry ack"),
            )
            .await;
    };
    let (retry_result, ()) = tokio::join!(retry_send, retry_apply);
    assert!(matches!(retry_result, KernelResult::CommandApplied { .. }));

    assert!(matches!(
        submit_pty_control(
            &mut sender,
            "settled-replay",
            KernelRequest::StartPtySession {
                envelope: first_envelope,
            },
        )
        .await,
        KernelResult::CommandApplied { .. }
    ));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), manager.recv())
            .await
            .is_err(),
        "a settled replay must not deliver twice"
    );

    let later_envelope = envelope_in(
        gwk_kernel::SYSTEM_PROJECT,
        "start-later-lifetime",
        actor("operator"),
        &start,
    );
    let later_send = submit_pty_control(
        &mut sender,
        "start-later-lifetime",
        KernelRequest::StartPtySession {
            envelope: later_envelope,
        },
    );
    let later_apply = async {
        let control = tokio::time::timeout(std::time::Duration::from_secs(3), manager.recv())
            .await
            .expect("the later lifetime reaches the manager")
            .expect("manager remains open");
        let ServerControl::PtyStart {
            delivery_id,
            session_id,
            ..
        } = control
        else {
            panic!("unexpected later start: {control:?}");
        };
        assert_eq!(session_id.as_str(), "reusable");
        manager
            .send(
                &serde_json::to_string(&gwk_domain::ClientControl::PtyDeliveryAck {
                    delivery_id,
                    result: gwk_domain::protocol::PtyDeliveryResult::Applied,
                })
                .expect("serialize later ack"),
            )
            .await;
    };
    let (later_result, ()) = tokio::join!(later_send, later_apply);
    assert!(matches!(later_result, KernelResult::CommandApplied { .. }));

    let mut colliding_envelope = envelope_in(
        gwk_kernel::SYSTEM_PROJECT,
        "start-command-id-collision",
        actor("operator"),
        &start,
    );
    colliding_envelope.command_id = gwk_domain::CommandId::new("cmd-system-start-later-lifetime");
    assert!(matches!(
        submit_pty_control(
            &mut sender,
            "start-command-id-collision",
            KernelRequest::StartPtySession {
                envelope: colliding_envelope,
            },
        )
        .await,
        KernelResult::Error {
            code: KernelErrorCode::IdempotencyConflict,
            ..
        }
    ));

    let starts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gwk.event WHERE event_type = 'pty_session_start_requested'",
    )
    .fetch_one(&pool)
    .await
    .expect("start event count");
    assert_eq!(
        starts, 2,
        "the retry is one event and the later lifetime another"
    );

    drop(manager);
    drop(sender);
    served.close().await;
    drop(pool);
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL"]
async fn a_published_pty_session_is_served_end_to_end_and_a_foreign_writer_is_refused() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_sealed_store(&maintenance, "wire_pty", 8).await;
    let served = Running::open(store, "pty").await;

    // Two connections on purpose: the publishing host must not be the viewer,
    // or the batch the publish causes and the publish's own response arrive on
    // one wire with nothing in the protocol deciding their order.
    let mut host = served.client().await;
    let mut viewer = served.client().await;

    // The host claims the session with its screen at revision 0.
    let frame = serde_json::to_string(&pty_frame(2, 4)).expect("serialize frame");
    match host
        .ask(
            "h-seed",
            &format!(
                r#"{{"type":"pty_publish_snapshot","session_id":"pty-console","seq":"0","frame":{frame}}}"#
            ),
        )
        .await
    {
        KernelResult::PtyPublished { session_id } => {
            assert_eq!(session_id.as_str(), "pty-console");
        }
        other => panic!("{other:?}"),
    }

    // A snapshot serves the published screen back, byte for byte.
    let snapshot_generation = match viewer
        .ask(
            "v-snap",
            r#"{"type":"pty_snapshot","session_id":"pty-console"}"#,
        )
        .await
    {
        KernelResult::PtySnapshot {
            session_id,
            generation,
            seq,
            frame,
        } => {
            assert_eq!(session_id.as_str(), "pty-console");
            assert_eq!(seq, gwk_domain::ids::PtyFrameSeq::new(0));
            let cells = frame.cells().expect("a served snapshot expands");
            assert_eq!(cells.len(), 2);
            assert!(cells.iter().all(|row| row.len() == 4));
            generation
        }
        other => panic!("{other:?}"),
    };

    // A fresh attach answers the dimensions and the revision deltas resume
    // from, before any batch.
    let first_generation = match viewer
        .ask(
            "v-attach",
            r#"{"type":"pty_attach","session_id":"pty-console"}"#,
        )
        .await
    {
        KernelResult::PtyAttached {
            session_id,
            generation,
            rows,
            cols,
            cursor,
        } => {
            assert_eq!(session_id.as_str(), "pty-console");
            assert_eq!(generation, snapshot_generation);
            assert_eq!((rows, cols), (2, 4));
            assert_eq!(cursor, Some(gwk_domain::ids::PtyFrameSeq::new(0)));
            generation
        }
        other => panic!("{other:?}"),
    };

    // The host moves the screen; the viewer's live stream carries the batch,
    // tagged with the attach's own request id.
    let update = serde_json::to_string(&gwk_domain::frame::PtyDelta::CellsChanged {
        styles: vec![pty_cell("x").style],
        updates: vec![gwk_domain::frame::PtyCellUpdate {
            row: 0,
            col: 0,
            glyph: "x".to_owned(),
            style: 0,
        }],
    })
    .expect("serialize delta");
    match host
        .ask(
            "h-delta",
            &format!(
                r#"{{"type":"pty_publish_deltas","session_id":"pty-console","seq":"1","deltas":[{update}]}}"#
            ),
        )
        .await
    {
        KernelResult::PtyPublished { .. } => {}
        other => panic!("{other:?}"),
    }
    let batch = tokio::time::timeout(std::time::Duration::from_secs(3), viewer.recv())
        .await
        .expect("a published batch reaches an attached viewer promptly")
        .expect("the connection stayed open");
    match batch {
        ServerControl::PtyDeltaBatch {
            request_id,
            session_id,
            generation,
            deltas,
            seq,
        } => {
            assert_eq!(request_id.as_str(), "v-attach");
            assert_eq!(session_id.as_str(), "pty-console");
            assert_eq!(generation, first_generation);
            assert_eq!(seq, gwk_domain::ids::PtyFrameSeq::new(1));
            // The batch's CONTENT survives the trip, not just its count.
            match &deltas[..] {
                [gwk_domain::frame::PtyDelta::CellsChanged { styles, updates }] => {
                    assert_eq!(styles.len(), 1, "the batch table rides the wire whole");
                    assert_eq!(updates.len(), 1);
                    assert_eq!((updates[0].row, updates[0].col), (0, 0));
                    assert_eq!(updates[0].glyph, "x");
                    assert_eq!(updates[0].style, 0);
                }
                other => panic!("{other:?}"),
            }
        }
        other => panic!("{other:?}"),
    }

    // The viewer's connection never claimed the session, so its publish is
    // refused as `authority` — single-writer coherence, not identity (the
    // peer credential already settled identity at accept time), and not
    // `validation` either: the request's shape is fine, the actor is not.
    match viewer
        .ask(
            "v-poach",
            &format!(
                r#"{{"type":"pty_publish_snapshot","session_id":"pty-console","seq":"9","frame":{frame}}}"#
            ),
        )
        .await
    {
        KernelResult::Error { code, .. } => assert_eq!(code, KernelErrorCode::Authority),
        other => panic!("{other:?}"),
    }

    // The seeded negative the exit condition names: an attach to a session
    // nobody hosts is refused typed, on the same code an absent projection
    // answers with.
    match viewer
        .ask(
            "v-ghost",
            r#"{"type":"pty_attach","session_id":"pty-ghost"}"#,
        )
        .await
    {
        KernelResult::Error { code, message, .. } => {
            assert_eq!(code, KernelErrorCode::NotFound);
            assert!(message.contains("pty-ghost"), "{message}");
        }
        other => panic!("{other:?}"),
    }

    // Retire closes the viewer's stream typed, with the last revision that
    // actually reached it.
    match host
        .ask(
            "h-retire",
            r#"{"type":"pty_retire","session_id":"pty-console"}"#,
        )
        .await
    {
        KernelResult::PtyRetired { session_id } => {
            assert_eq!(session_id.as_str(), "pty-console");
        }
        other => panic!("{other:?}"),
    }
    let closed = tokio::time::timeout(std::time::Duration::from_secs(3), viewer.recv())
        .await
        .expect("a retire reaches attached viewers promptly")
        .expect("the connection stayed open");
    match closed {
        ServerControl::PtyStreamClosed {
            request_id,
            generation,
            code,
            last_seq,
        } => {
            assert_eq!(request_id.as_str(), "v-attach");
            assert_eq!(generation, first_generation);
            assert_eq!(code, KernelErrorCode::NotFound);
            assert_eq!(last_seq, Some(gwk_domain::ids::PtyFrameSeq::new(1)));
        }
        other => panic!("{other:?}"),
    }

    // And the id is gone from the request/response surface too.
    match viewer
        .ask(
            "v-after",
            r#"{"type":"pty_snapshot","session_id":"pty-console"}"#,
        )
        .await
    {
        KernelResult::Error { code, .. } => assert_eq!(code, KernelErrorCode::NotFound),
        other => panic!("{other:?}"),
    }

    // ---- The reclaim: a successor connection, the same id, a later head ----
    // The engine kept running while the host's connection was down, so the
    // reclaim seeds at revision 5 with revisions 1..=5 gone with the old entry.
    let mut host2 = served.client().await;
    match host2
        .ask(
            "h2-seed",
            &format!(
                r#"{{"type":"pty_publish_snapshot","session_id":"pty-console","seq":"5","frame":{frame}}}"#
            ),
        )
        .await
    {
        KernelResult::PtyPublished { .. } => {}
        other => panic!("{other:?}"),
    }

    // A viewer still holding the OLD life's cursor is reseeded at the head:
    // answering the cursor back would claim gap-free continuity over a hole.
    let second_generation = match viewer
        .ask(
            "v-re",
            &format!(
                r#"{{"type":"pty_attach","session_id":"pty-console","generation":"{first_generation}","cursor":"1"}}"#
            ),
        )
        .await
    {
        KernelResult::PtyAttached {
            generation,
            cursor,
            ..
        } => {
            assert_ne!(generation, first_generation);
            assert_eq!(
                cursor,
                Some(gwk_domain::ids::PtyFrameSeq::new(5)),
                "a stale cursor must be answered at the reclaim head — reseed"
            );
            generation
        }
        other => panic!("{other:?}"),
    };

    // The next batch reaches the reattached viewer live…
    match host2
        .ask(
            "h2-delta",
            &format!(
                r#"{{"type":"pty_publish_deltas","session_id":"pty-console","seq":"6","deltas":[{update}]}}"#
            ),
        )
        .await
    {
        KernelResult::PtyPublished { .. } => {}
        other => panic!("{other:?}"),
    }
    match tokio::time::timeout(std::time::Duration::from_secs(3), viewer.recv())
        .await
        .expect("the post-reclaim batch reaches the viewer")
        .expect("open")
    {
        ServerControl::PtyDeltaBatch {
            request_id,
            generation,
            seq,
            ..
        } => {
            assert_eq!(request_id.as_str(), "v-re");
            assert_eq!(generation, second_generation);
            assert_eq!(seq, gwk_domain::ids::PtyFrameSeq::new(6));
        }
        other => panic!("{other:?}"),
    }

    // …and REPLAYS to a second viewer whose cursor is inside the retained
    // window: the catch-up path itself crossing the wire, not just the live one.
    let mut viewer2 = served.client().await;
    match viewer2
        .ask(
            "v2-attach",
            &format!(
                r#"{{"type":"pty_attach","session_id":"pty-console","generation":"{second_generation}","cursor":"5"}}"#
            ),
        )
        .await
    {
        KernelResult::PtyAttached {
            generation,
            cursor,
            ..
        } => {
            assert_eq!(generation, second_generation);
            assert_eq!(cursor, Some(gwk_domain::ids::PtyFrameSeq::new(5)));
        }
        other => panic!("{other:?}"),
    }
    match tokio::time::timeout(std::time::Duration::from_secs(3), viewer2.recv())
        .await
        .expect("the retained batch replays to a cursor attach")
        .expect("open")
    {
        ServerControl::PtyDeltaBatch {
            request_id, seq, ..
        } => {
            assert_eq!(request_id.as_str(), "v2-attach");
            assert_eq!(seq, gwk_domain::ids::PtyFrameSeq::new(6));
        }
        other => panic!("{other:?}"),
    }

    // The publisher HANGS UP without retiring — the crashed-host path. Every
    // attached viewer is closed typed, exactly as an explicit retire closes.
    drop(host2);
    for (viewer, request_id) in [(&mut viewer, "v-re"), (&mut viewer2, "v2-attach")] {
        match tokio::time::timeout(std::time::Duration::from_secs(3), viewer.recv())
            .await
            .expect("a hangup retires promptly")
            .expect("open")
        {
            ServerControl::PtyStreamClosed {
                request_id: closed,
                generation,
                code,
                last_seq,
            } => {
                assert_eq!(closed.as_str(), request_id);
                assert_eq!(generation, second_generation);
                assert_eq!(code, KernelErrorCode::NotFound);
                assert_eq!(last_seq, Some(gwk_domain::ids::PtyFrameSeq::new(6)));
            }
            other => panic!("{other:?}"),
        }
    }

    // ---- PTY attaches share the subscription cap ----
    let mut host3 = served.client().await;
    match host3
        .ask(
            "h3-seed",
            &format!(
                r#"{{"type":"pty_publish_snapshot","session_id":"pty-cap","seq":"0","frame":{frame}}}"#
            ),
        )
        .await
    {
        KernelResult::PtyPublished { .. } => {}
        other => panic!("{other:?}"),
    }
    let mut viewer3 = served.client().await;
    for n in 0..gwk_domain::protocol::MAX_SUBSCRIPTIONS_PER_CONNECTION {
        match viewer3
            .ask(
                &format!("cap-{n}"),
                r#"{"type":"pty_attach","session_id":"pty-cap"}"#,
            )
            .await
        {
            KernelResult::PtyAttached { .. } => {}
            other => panic!("attach {n} under the cap: {other:?}"),
        }
    }
    match viewer3
        .ask(
            "cap-over",
            r#"{"type":"pty_attach","session_id":"pty-cap"}"#,
        )
        .await
    {
        KernelResult::Error { code, .. } => assert_eq!(code, KernelErrorCode::Overloaded),
        other => panic!("{other:?}"),
    }

    drop(host);
    drop(host3);
    drop(viewer);
    drop(viewer2);
    drop(viewer3);
    served.close().await;
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn receipted_pty_input_reaches_the_owning_host_once_across_the_real_wire() {
    use gwk_domain::command::KernelCommand;
    use gwk_domain::ids::ByteCount;
    use gwk_domain::protocol::KernelRequest;
    use sqlx::Row;

    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wire_pty_input", 8).await;
    let pool = store.pool().clone();
    let served = Running::open(store, "ptyinput").await;
    let mut host = served
        .client_with_capabilities(&[PTY_INPUT_CAPABILITY])
        .await;
    let mut sender = served
        .client_with_capabilities(&[PTY_INPUT_CAPABILITY])
        .await;

    let frame = serde_json::to_string(&pty_frame(2, 4)).expect("serialize frame");
    match host
        .ask(
            "seed",
            &format!(
                r#"{{"type":"pty_publish_snapshot","session_id":"console","seq":"0","frame":{frame}}}"#
            ),
        )
        .await
    {
        KernelResult::PtyPublished { .. } => {}
        other => panic!("seed: {other:?}"),
    }
    let generation = match sender
        .ask(
            "generation",
            r#"{"type":"pty_snapshot","session_id":"console"}"#,
        )
        .await
    {
        KernelResult::PtySnapshot { generation, .. } => generation,
        other => panic!("generation: {other:?}"),
    };

    let bytes = [0x00, 0xff, b'\n'];
    let command = KernelCommand::SendPtyInput {
        pty_session_id: gwk_domain::PtySessionId::new("console"),
        generation: generation.clone(),
        byte_count: ByteCount::new(bytes.len() as u64),
    };
    // The session was host-published, so it is owned by the system project.
    let envelope = envelope_in(
        gwk_kernel::SYSTEM_PROJECT,
        "wire-input",
        actor("operator"),
        &command,
    );
    let request = KernelRequest::SendPtyInput {
        envelope: envelope.clone(),
        data_base64: PtyInputData::new(BASE64_STANDARD.encode(bytes)),
    };
    let request = serde_json::to_string(&request).expect("serialize input request");
    let input_send = sender.ask("send", &request);
    let input_apply = async {
        let control = tokio::time::timeout(std::time::Duration::from_secs(3), host.recv())
            .await
            .expect("input reaches the owning host promptly")
            .expect("host connection remains open");
        let delivery_id = match control {
            ServerControl::PtyInput {
                delivery_id,
                command_id,
                session_id,
                generation: delivered_generation,
                byte_size,
                data_base64,
            } => {
                assert_eq!(command_id, envelope.command_id);
                assert_eq!(session_id.as_str(), "console");
                assert_eq!(delivered_generation, generation);
                assert_eq!(byte_size, ByteCount::new(bytes.len() as u64));
                assert_eq!(
                    BASE64_STANDARD
                        .decode(data_base64.as_str())
                        .expect("delivery base64"),
                    bytes
                );
                delivery_id
            }
            other => panic!("owner received {other:?}"),
        };
        host.send(
            &serde_json::to_string(&gwk_domain::ClientControl::PtyDeliveryAck {
                delivery_id,
                result: gwk_domain::protocol::PtyDeliveryResult::Applied,
            })
            .expect("serialize input ack"),
        )
        .await;
    };
    let (input_result, ()) = tokio::join!(input_send, input_apply);
    match input_result {
        KernelResult::CommandApplied { events, .. } => {
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].event_type, "pty_input_requested");
            assert!(
                events[0].payload.get("data_base64").is_none(),
                "terminal bytes must not enter the immutable event"
            );
        }
        other => panic!("send: {other:?}"),
    }

    // A retry is the same logical call. It returns the original command result
    // but must not type the bytes a second time.
    assert!(matches!(
        sender.ask("retry", &request).await,
        KernelResult::CommandApplied { .. }
    ));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), host.recv())
            .await
            .is_err(),
        "an idempotent replay must not redeliver input"
    );

    let event_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gwk.event WHERE event_type = 'pty_input_requested'",
    )
    .fetch_one(&pool)
    .await
    .expect("input event count");
    assert_eq!(event_count, 1);
    let receipt = sqlx::query(
        "SELECT actor->>'kind' AS actor_kind, action, subject_id, observed_basis \
         FROM gwk.receipt WHERE id = 'receipt:system:wire-input'",
    )
    .fetch_one(&pool)
    .await
    .expect("input receipt");
    assert_eq!(receipt.get::<String, _>("actor_kind"), "operator");
    assert_eq!(receipt.get::<String, _>("action"), "pty_input");
    assert_eq!(
        receipt.get::<String, _>("subject_id"),
        format!("console:{generation}")
    );
    assert_eq!(
        receipt.get::<String, _>("observed_basis"),
        "operator authority; byte_count=3"
    );

    drop(host);
    drop(sender);
    served.close().await;
    drop(pool);
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_pending_input_retry_cannot_replace_the_original_bytes() {
    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wire_pty_input_binding", 8).await;
    let pool = store.pool().clone();
    let served = Running::open(store, "ptyinputbinding").await;
    let mut host = served
        .client_with_capabilities(&[PTY_INPUT_CAPABILITY])
        .await;
    let mut sender = served
        .client_with_capabilities(&[PTY_INPUT_CAPABILITY])
        .await;
    let frame = serde_json::to_string(&pty_frame(2, 4)).expect("serialize frame");
    assert!(matches!(
        host.ask(
            "seed",
            &format!(
                r#"{{"type":"pty_publish_snapshot","session_id":"console","seq":"0","frame":{frame}}}"#
            ),
        )
        .await,
        KernelResult::PtyPublished { .. }
    ));
    let generation = match sender
        .ask(
            "generation",
            r#"{"type":"pty_snapshot","session_id":"console"}"#,
        )
        .await
    {
        KernelResult::PtySnapshot { generation, .. } => generation,
        other => panic!("generation: {other:?}"),
    };
    let command = KernelCommand::SendPtyInput {
        pty_session_id: gwk_domain::PtySessionId::new("console"),
        generation: generation.clone(),
        byte_count: gwk_domain::ByteCount::new(1),
    };
    let envelope = envelope_in(
        gwk_kernel::SYSTEM_PROJECT,
        "bound-input",
        actor("operator"),
        &command,
    );
    let request = |byte| KernelRequest::SendPtyInput {
        envelope: envelope.clone(),
        data_base64: PtyInputData::new(BASE64_STANDARD.encode([byte])),
    };

    let first = serde_json::to_string(&request(b'a')).expect("serialize first input");
    let first_send = sender.ask("first", &first);
    let first_apply = async {
        let control = host.recv().await.expect("first delivery");
        let ServerControl::PtyInput { delivery_id, .. } = control else {
            panic!("unexpected delivery: {control:?}");
        };
        host.send(
            &serde_json::to_string(&ClientControl::PtyDeliveryAck {
                delivery_id,
                result: PtyDeliveryResult::Refused {
                    code: KernelErrorCode::Overloaded,
                    message: "retry later".to_owned(),
                },
            })
            .expect("serialize refusal"),
        )
        .await;
    };
    let (first_result, ()) = tokio::join!(first_send, first_apply);
    assert!(matches!(first_result, KernelResult::Error { .. }));

    let replacement = serde_json::to_string(&request(b'b')).expect("serialize replacement");
    match sender.ask("replacement", &replacement).await {
        KernelResult::Error { code, .. } => assert_eq!(code, KernelErrorCode::IdempotencyConflict),
        other => panic!("replacement carrier: {other:?}"),
    }
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), host.recv())
            .await
            .is_err(),
        "a conflicting carrier must not reach the host"
    );
    let binding: Option<Vec<u8>> = sqlx::query_scalar(
        "SELECT input_binding FROM gwk_internal.pty_delivery WHERE idempotency_key = 'bound-input'",
    )
    .fetch_one(&pool)
    .await
    .expect("input binding");
    assert!(
        binding.is_some(),
        "the original carrier must be durably bound"
    );

    drop(host);
    drop(sender);
    served.close().await;
    drop(pool);
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn pty_input_authority_honors_live_grants_revocation_and_actor_kind() {
    use gwk_domain::command::PTY_INPUT_ACTION_CLASS;
    use gwk_domain::ids::{AuthorityGrantId, Timestamp};
    use sqlx::Row;

    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wire_pty_input_auth", 8).await;
    let pool = store.pool().clone();
    let served = Running::open(store, "ptyinputauth").await;
    let mut host = served
        .client_with_capabilities(&[PTY_INPUT_CAPABILITY])
        .await;
    let mut sender = served
        .client_with_capabilities(&[PTY_INPUT_CAPABILITY])
        .await;
    let frame = serde_json::to_string(&pty_frame(2, 4)).expect("serialize frame");

    match host
        .ask(
            "seed",
            &format!(
                r#"{{"type":"pty_publish_snapshot","session_id":"console","seq":"0","frame":{frame}}}"#
            ),
        )
        .await
    {
        KernelResult::PtyPublished { .. } => {}
        other => panic!("seed: {other:?}"),
    }
    let generation = match sender
        .ask(
            "generation",
            r#"{"type":"pty_snapshot","session_id":"console"}"#,
        )
        .await
    {
        KernelResult::PtySnapshot { generation, .. } => generation,
        other => panic!("generation: {other:?}"),
    };
    let subject = format!("console:{generation}");

    match send_pty_input(
        &mut sender,
        "ungranted-request",
        "ungranted",
        "orchestrator",
        "console",
        &generation,
        b"a",
    )
    .await
    {
        KernelResult::Error { code, message, .. } => {
            assert_eq!(code, KernelErrorCode::Authority, "{message}")
        }
        other => panic!("ungranted orchestrator: {other:?}"),
    }

    let grant = gwk_domain::KernelCommand::GrantAuthority {
        authority_grant_id: AuthorityGrantId::new("pty-input-live"),
        grantee: actor("orchestrator"),
        action_class: PTY_INPUT_ACTION_CLASS.to_owned(),
        scope: Some(subject.clone()),
        expires_at: Some(Timestamp::new("2099-01-01T00:00:00Z")),
    };
    assert!(matches!(
        submit_kernel_command(
            &mut sender,
            "grant-request",
            "grant-input",
            "operator",
            &grant,
        )
        .await,
        KernelResult::CommandApplied { .. }
    ));
    let granted_send = send_pty_input(
        &mut sender,
        "granted-request",
        "granted",
        "orchestrator",
        "console",
        &generation,
        b"b",
    );
    let granted_apply = async {
        let control = tokio::time::timeout(std::time::Duration::from_secs(3), host.recv())
            .await
            .expect("granted input reaches host")
            .expect("host remains open");
        let ServerControl::PtyInput {
            delivery_id,
            data_base64,
            ..
        } = control
        else {
            panic!("unexpected granted delivery: {control:?}");
        };
        assert_eq!(
            BASE64_STANDARD
                .decode(data_base64.as_str())
                .expect("delivery"),
            b"b"
        );
        host.send(
            &serde_json::to_string(&gwk_domain::ClientControl::PtyDeliveryAck {
                delivery_id,
                result: gwk_domain::protocol::PtyDeliveryResult::Applied,
            })
            .expect("serialize granted ack"),
        )
        .await;
    };
    let (granted_result, ()) = tokio::join!(granted_send, granted_apply);
    assert!(matches!(
        granted_result,
        KernelResult::CommandApplied { .. }
    ));

    let revoke = gwk_domain::KernelCommand::RevokeAuthority {
        authority_grant_id: AuthorityGrantId::new("pty-input-live"),
        reason: Some("operator revoked terminal input".to_owned()),
    };
    assert!(matches!(
        submit_kernel_command(
            &mut sender,
            "revoke-request",
            "revoke-input",
            "operator",
            &revoke,
        )
        .await,
        KernelResult::CommandApplied { .. }
    ));
    match send_pty_input(
        &mut sender,
        "revoked-request",
        "revoked",
        "orchestrator",
        "console",
        &generation,
        b"c",
    )
    .await
    {
        KernelResult::Error { code, .. } => assert_eq!(code, KernelErrorCode::Authority),
        other => panic!("revoked orchestrator: {other:?}"),
    }

    let expired = gwk_domain::KernelCommand::GrantAuthority {
        authority_grant_id: AuthorityGrantId::new("pty-input-expired"),
        grantee: actor("orchestrator"),
        action_class: PTY_INPUT_ACTION_CLASS.to_owned(),
        scope: Some(subject.clone()),
        expires_at: Some(Timestamp::new("2000-01-01T00:00:00Z")),
    };
    assert!(matches!(
        submit_kernel_command(
            &mut sender,
            "expired-grant-request",
            "grant-expired",
            "operator",
            &expired,
        )
        .await,
        KernelResult::CommandApplied { .. }
    ));
    match send_pty_input(
        &mut sender,
        "expired-request",
        "expired",
        "orchestrator",
        "console",
        &generation,
        b"d",
    )
    .await
    {
        KernelResult::Error { code, .. } => assert_eq!(code, KernelErrorCode::Authority),
        other => panic!("expired orchestrator: {other:?}"),
    }

    let engine_grant = gwk_domain::KernelCommand::GrantAuthority {
        authority_grant_id: AuthorityGrantId::new("pty-input-engine"),
        grantee: actor("engine"),
        action_class: PTY_INPUT_ACTION_CLASS.to_owned(),
        scope: Some(subject.clone()),
        expires_at: None,
    };
    assert!(matches!(
        submit_kernel_command(
            &mut sender,
            "engine-grant-request",
            "grant-engine",
            "operator",
            &engine_grant,
        )
        .await,
        KernelResult::CommandApplied { .. }
    ));
    match send_pty_input(
        &mut sender,
        "engine-request",
        "engine",
        "engine",
        "console",
        &generation,
        b"e",
    )
    .await
    {
        KernelResult::Error { code, .. } => assert_eq!(code, KernelErrorCode::Authority),
        other => panic!("granted engine: {other:?}"),
    }
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), host.recv())
            .await
            .is_err(),
        "only the one granted orchestrator send reaches the host"
    );

    let receipts = sqlx::query(
        "SELECT id, actor->>'kind' AS actor_kind, subject_id, observed_basis \
         FROM gwk.receipt WHERE action = 'pty_input' ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .expect("input receipts");
    assert_eq!(receipts.len(), 5, "one receipt per logical send call");
    assert!(receipts.iter().all(|row| {
        row.get::<String, _>("subject_id") == subject
            && row
                .get::<String, _>("observed_basis")
                .ends_with("byte_count=1")
    }));
    let basis = |key: &str| {
        receipts
            .iter()
            .find(|row| row.get::<String, _>("id") == format!("receipt:system:{key}"))
            .map(|row| {
                (
                    row.get::<String, _>("actor_kind"),
                    row.get::<String, _>("observed_basis"),
                )
            })
            .expect("receipt by key")
    };
    assert_eq!(
        basis("granted"),
        (
            "orchestrator".to_owned(),
            "matching unexpired scoped grant; byte_count=1".to_owned(),
        )
    );
    for key in ["ungranted", "revoked", "expired"] {
        assert_eq!(
            basis(key),
            (
                "orchestrator".to_owned(),
                "no matching unexpired scoped grant; byte_count=1".to_owned(),
            )
        );
    }
    assert_eq!(
        basis("engine"),
        (
            "engine".to_owned(),
            "actor kind is neither operator nor orchestrator; byte_count=1".to_owned(),
        )
    );
    let input_events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gwk.event WHERE event_type = 'pty_input_requested'",
    )
    .fetch_one(&pool)
    .await
    .expect("input event count");
    assert_eq!(input_events, 1, "only the granted send mutates");

    drop(host);
    drop(sender);
    served.close().await;
    drop(pool);
    drop_database(&maintenance, &name).await;
}

#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn pty_input_refuses_stale_and_missing_generations_with_receipts() {
    use sqlx::Row;

    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wire_pty_input_generation", 8).await;
    let pool = store.pool().clone();
    let served = Running::open(store, "ptyinputgeneration").await;
    let frame = serde_json::to_string(&pty_frame(2, 4)).expect("serialize frame");
    let publish = format!(
        r#"{{"type":"pty_publish_snapshot","session_id":"console","seq":"0","frame":{frame}}}"#
    );
    let mut host = served
        .client_with_capabilities(&[PTY_INPUT_CAPABILITY])
        .await;
    let mut sender = served
        .client_with_capabilities(&[PTY_INPUT_CAPABILITY])
        .await;

    assert!(matches!(
        host.ask("old-seed", &publish).await,
        KernelResult::PtyPublished { .. }
    ));
    let old = match sender
        .ask(
            "old-generation",
            r#"{"type":"pty_snapshot","session_id":"console"}"#,
        )
        .await
    {
        KernelResult::PtySnapshot { generation, .. } => generation,
        other => panic!("old generation: {other:?}"),
    };
    assert!(matches!(
        host.ask(
            "old-retire",
            r#"{"type":"pty_retire","session_id":"console"}"#,
        )
        .await,
        KernelResult::PtyRetired { .. }
    ));

    let mut current_host = served
        .client_with_capabilities(&[PTY_INPUT_CAPABILITY])
        .await;
    assert!(matches!(
        current_host.ask("current-seed", &publish).await,
        KernelResult::PtyPublished { .. }
    ));
    let current = match sender
        .ask(
            "current-generation",
            r#"{"type":"pty_snapshot","session_id":"console"}"#,
        )
        .await
    {
        KernelResult::PtySnapshot { generation, .. } => generation,
        other => panic!("current generation: {other:?}"),
    };
    assert_ne!(old, current);

    match send_pty_input(
        &mut sender,
        "stale-request",
        "stale",
        "operator",
        "console",
        &old,
        b"x",
    )
    .await
    {
        KernelResult::Error { code, detail, .. } => {
            assert_eq!(code, KernelErrorCode::StaleVersion);
            assert_eq!(detail.expect("stale detail")["state"], "closed");
        }
        other => panic!("stale send: {other:?}"),
    }
    match send_pty_input(
        &mut sender,
        "missing-request",
        "missing",
        "operator",
        "console",
        &gwk_domain::PtySessionGeneration::new("missing"),
        b"y",
    )
    .await
    {
        KernelResult::Error { code, message, .. } => {
            assert_eq!(code, KernelErrorCode::NotFound);
            assert!(message.contains("console:missing"), "{message}");
        }
        other => panic!("missing send: {other:?}"),
    }
    let current_send = send_pty_input(
        &mut sender,
        "current-request",
        "current",
        "operator",
        "console",
        &current,
        b"z",
    );
    let current_apply = async {
        let control = tokio::time::timeout(std::time::Duration::from_secs(3), current_host.recv())
            .await
            .expect("current input reaches current host")
            .expect("current host remains open");
        let ServerControl::PtyInput {
            delivery_id,
            data_base64,
            ..
        } = control
        else {
            panic!("unexpected current delivery: {control:?}");
        };
        assert_eq!(
            BASE64_STANDARD
                .decode(data_base64.as_str())
                .expect("delivery"),
            b"z"
        );
        current_host
            .send(
                &serde_json::to_string(&gwk_domain::ClientControl::PtyDeliveryAck {
                    delivery_id,
                    result: gwk_domain::protocol::PtyDeliveryResult::Applied,
                })
                .expect("serialize current ack"),
            )
            .await;
    };
    let (current_result, ()) = tokio::join!(current_send, current_apply);
    assert!(matches!(
        current_result,
        KernelResult::CommandApplied { .. }
    ));

    let receipts = sqlx::query(
        "SELECT subject_id, observed_basis FROM gwk.receipt \
         WHERE action = 'pty_input' ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .expect("generation receipts");
    assert_eq!(receipts.len(), 3);
    assert!(receipts.iter().any(|row| {
        row.get::<String, _>("subject_id") == format!("console:{old}")
            && row
                .get::<String, _>("observed_basis")
                .contains("refused=stale_version")
    }));
    assert!(receipts.iter().any(|row| {
        row.get::<String, _>("subject_id") == "console:missing"
            && row
                .get::<String, _>("observed_basis")
                .contains("refused=not_found")
    }));
    assert!(receipts.iter().any(|row| {
        row.get::<String, _>("subject_id") == format!("console:{current}")
            && row
                .get::<String, _>("observed_basis")
                .starts_with("operator authority")
    }));
    let input_events: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM gwk.event WHERE event_type = 'pty_input_requested'",
    )
    .fetch_one(&pool)
    .await
    .expect("input event count");
    assert_eq!(input_events, 1, "only the current generation mutates");

    drop(host);
    drop(current_host);
    drop(sender);
    served.close().await;
    drop(pool);
    drop_database(&maintenance, &name).await;
}

/// The P17 receipts, proven across the real wire: one full lifecycle lands
/// exactly its four ledger events, a hangup close carries its provenance in
/// the idempotency key, and every receipt is kernel-authored.
#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn pty_lifecycle_receipts_reach_the_ledger_with_their_provenance() {
    use sqlx::Row;

    let maintenance = maintenance_pool().await;
    // Activated, not sealed: a sealed kernel admits no business command, so
    // its receipts are refused-and-logged (that graceful degradation IS the
    // log-and-continue design; the live estate serves activated).
    let (name, store) = fresh_store(&maintenance, "wire_pty_receipts", 8).await;
    let pool = store.pool().clone();
    let served = Running::open(store, "ptyrcpt").await;

    let mut host = served.client().await;
    let mut viewer = served.client().await;
    let frame = serde_json::to_string(&pty_frame(2, 4)).expect("serialize frame");

    // Session 1: the typed lifecycle — publish, attach, typed retire.
    match host
        .ask(
            "r1-seed",
            &format!(
                r#"{{"type":"pty_publish_snapshot","session_id":"pty-r1","seq":"0","frame":{frame}}}"#
            ),
        )
        .await
    {
        KernelResult::PtyPublished { .. } => {}
        other => panic!("{other:?}"),
    }
    match viewer
        .ask("r1-att", r#"{"type":"pty_attach","session_id":"pty-r1"}"#)
        .await
    {
        KernelResult::PtyAttached { .. } => {}
        other => panic!("{other:?}"),
    }
    match host
        .ask("r1-ret", r#"{"type":"pty_retire","session_id":"pty-r1"}"#)
        .await
    {
        KernelResult::PtyRetired { .. } => {}
        other => panic!("{other:?}"),
    }

    // Session 2: published, then the host connection goes away — the hangup
    // sweep is the close author.
    match host
        .ask(
            "r2-seed",
            &format!(
                r#"{{"type":"pty_publish_snapshot","session_id":"pty-r2","seq":"0","frame":{frame}}}"#
            ),
        )
        .await
    {
        KernelResult::PtyPublished { .. } => {}
        other => panic!("{other:?}"),
    }
    drop(host);

    // The detach and the hangup close land asynchronously; poll to quiescence.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let r1 = sqlx::query(
            "SELECT state, attach_count, detach_count FROM gwk.pty_session WHERE id LIKE 'pty-r1:%'",
        )
        .fetch_optional(&pool)
        .await
        .expect("read r1");
        let r2 = sqlx::query_scalar::<_, String>(
            "SELECT state FROM gwk.pty_session WHERE id LIKE 'pty-r2:%'",
        )
        .fetch_optional(&pool)
        .await
        .expect("read r2");
        let settled = r1.as_ref().is_some_and(|row| {
            row.get::<String, _>("state") == "closed"
                && row.get::<i64, _>("attach_count") == 1
                && row.get::<i64, _>("detach_count") == 1
        }) && r2.as_deref() == Some("closed");
        if settled {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "receipts did not settle: r1 {r1:?} r2 {r2:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // Session 1's ledger trail: exactly the four lifecycle events.
    let types: Vec<String> = sqlx::query_scalar(
        "SELECT event_type FROM gwk.event \
         WHERE aggregate_type = 'pty_session' AND aggregate_id LIKE 'pty-r1:%' \
         ORDER BY seq",
    )
    .fetch_all(&pool)
    .await
    .expect("r1 events");
    // The open leads and the attach precedes its detach; the typed close and
    // the detach RACE (the retire drops the broadcast, the viewer task emits
    // on its own schedule) — either order is legal, so only the set and the
    // causal edges are asserted.
    assert_eq!(
        types.first().map(String::as_str),
        Some("pty_session_opened")
    );
    let mut sorted = types.clone();
    sorted.sort();
    assert_eq!(
        sorted,
        [
            "pty_attach_recorded",
            "pty_detach_recorded",
            "pty_session_closed",
            "pty_session_opened"
        ],
        "one full lifecycle is exactly these four events: {types:?}"
    );
    let position = |t: &str| types.iter().position(|x| x == t).expect(t);
    assert!(position("pty_attach_recorded") < position("pty_detach_recorded"));

    // Session 2's close carries the hangup provenance, and every receipt is
    // kernel-authored.
    let row = sqlx::query(
        "SELECT idempotency_key, actor->>'kind' AS actor_kind FROM gwk.event \
         WHERE aggregate_type = 'pty_session' AND aggregate_id LIKE 'pty-r2:%' \
           AND event_type = 'pty_session_closed'",
    )
    .fetch_one(&pool)
    .await
    .expect("r2 close event");
    let key: String = row.get("idempotency_key");
    assert!(key.ends_with(":hangup"), "close key {key} lacks provenance");
    assert_eq!(row.get::<String, _>("actor_kind"), "kernel");

    drop(viewer);
    served.close().await;
    drop(pool);
    drop_database(&maintenance, &name).await;
}

/// The two 2026-08-11 sitting findings, pinned. Two connections number their
/// requests independently, so both attaches here arrive as `gw-1`: with a
/// request-only idempotency key the second refused as a conflict, and a client
/// that hung up without detaching lost its detach receipt to the stream task's
/// abort. The key now embeds the connection and the abort itself emits.
#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn concurrent_and_hung_up_attaches_all_reach_the_counters() {
    use sqlx::Row;

    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wire_pty_concurrent", 8).await;
    let pool = store.pool().clone();
    let served = Running::open(store, "ptyconc").await;

    let mut host = served.client().await;
    let mut viewer_a = served.client().await;
    let mut viewer_b = served.client().await;
    let frame = serde_json::to_string(&pty_frame(2, 4)).expect("serialize frame");

    match host
        .ask(
            "seed",
            &format!(
                r#"{{"type":"pty_publish_snapshot","session_id":"pty-c1","seq":"0","frame":{frame}}}"#
            ),
        )
        .await
    {
        KernelResult::PtyPublished { .. } => {}
        other => panic!("{other:?}"),
    }

    // The SAME request id from two connections — each client's first request.
    for viewer in [&mut viewer_a, &mut viewer_b] {
        match viewer
            .ask("gw-1", r#"{"type":"pty_attach","session_id":"pty-c1"}"#)
            .await
        {
            KernelResult::PtyAttached { .. } => {}
            other => panic!("{other:?}"),
        }
    }

    let counters = |pool: sqlx::PgPool| async move {
        let row = sqlx::query(
            "SELECT state, attach_count, detach_count \
             FROM gwk.pty_session WHERE id LIKE 'pty-c1:%'",
        )
        .fetch_optional(&pool)
        .await
        .expect("read pty-c1");
        row.map(|row| {
            (
                row.get::<String, _>("state"),
                row.get::<i64, _>("attach_count"),
                row.get::<i64, _>("detach_count"),
            )
        })
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let state = counters(pool.clone()).await;
        if state.as_ref().is_some_and(|s| s.1 == 2) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "both attaches must count despite the shared request id: {state:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // Client hangup, no clean detach: the streams are aborted with their
    // connections, and the abort guard is what carries the receipts out.
    drop(viewer_a);
    drop(viewer_b);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let state = counters(pool.clone()).await;
        if state.as_ref().is_some_and(|s| s.2 == 2) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "hung-up attaches must still land their detaches: {state:?}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let (state, attaches, detaches) = counters(pool.clone()).await.expect("settled row");
    assert_eq!((state.as_str(), attaches, detaches), ("running", 2, 2));

    drop(host);
    served.close().await;
    drop(pool);
    drop_database(&maintenance, &name).await;
}

/// The wedge that took a kernel restart to find, pinned: session ids are
/// REUSED — the estate's resident session is always `console` — and each
/// lifetime must land its own ledger row. Before per-lifetime aggregate ids,
/// the second open was a version-0 create against the first lifetime's
/// surviving row: refused as StaleVersion, dropped silently, mirror wedged.
#[tokio::test]
#[ignore = "needs a PostgreSQL; see tests/common/mod.rs"]
async fn a_reused_session_id_gets_a_row_per_lifetime() {
    use sqlx::Row;

    let maintenance = maintenance_pool().await;
    let (name, store) = fresh_store(&maintenance, "wire_pty_lifetimes", 8).await;
    let pool = store.pool().clone();
    let served = Running::open(store, "ptylife").await;
    let frame = serde_json::to_string(&pty_frame(2, 4)).expect("serialize frame");
    let publish = format!(
        r#"{{"type":"pty_publish_snapshot","session_id":"pty-l1","seq":"0","frame":{frame}}}"#
    );

    // Lifetime 1: published, then the host hangs up — the sweep closes it.
    let mut host = served.client().await;
    match host.ask("l1-seed", &publish).await {
        KernelResult::PtyPublished { .. } => {}
        other => panic!("{other:?}"),
    }
    drop(host);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let closed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM gwk.pty_session \
             WHERE id LIKE 'pty-l1:%' AND state = 'closed'",
        )
        .fetch_one(&pool)
        .await
        .expect("count closed");
        if closed == 1 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "lifetime 1 must close on hangup"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    // Lifetime 2: the SAME session id from a fresh host. Its open must land
    // as its own row beside the closed one, not wedge against it.
    let mut host = served.client().await;
    match host.ask("l2-seed", &publish).await {
        KernelResult::PtyPublished { .. } => {}
        other => panic!("{other:?}"),
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let rows = sqlx::query(
            "SELECT id, state, generation FROM gwk.pty_session \
             WHERE id LIKE 'pty-l1:%' ORDER BY opened_at",
        )
        .fetch_all(&pool)
        .await
        .expect("lifetime rows");
        if rows.len() == 2 {
            assert_eq!(rows[0].get::<String, _>("state"), "closed");
            assert_eq!(rows[1].get::<String, _>("state"), "running");
            let g1 = rows[0].get::<String, _>("generation");
            let g2 = rows[1].get::<String, _>("generation");
            assert_ne!(g1, g2, "each lifetime carries its own generation");
            assert_eq!(rows[0].get::<String, _>("id"), format!("pty-l1:{g1}"));
            assert_eq!(rows[1].get::<String, _>("id"), format!("pty-l1:{g2}"));
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the reused id must open a second row: {} rows",
            rows.len()
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }

    drop(host);
    served.close().await;
    drop(pool);
    drop_database(&maintenance, &name).await;
}
