//! Live subscriptions: the half of the protocol that arrives unasked.
//!
//! A subscription is a cursor and a loop. It reads forward from where the
//! consumer last was, sends what it finds, and then waits for a reason to look
//! again. Everything interesting is in what counts as a reason, and in what
//! happens when the consumer stops keeping up.
//!
//! # Two wake-ups, because one of them is allowed to fail
//!
//! The append path already queues a `pg_notify` INSIDE its transaction, so a
//! wake-up is delivered only if the append committed and never announces one
//! that rolled back. That is the fast path and it is the normal path.
//!
//! It is not a guarantee. A notification can be lost — a dropped listener
//! connection, a backend restart, a listener that had not reconnected yet — and
//! PostgreSQL will not redeliver it. On a busy log that heals itself, because
//! the next append wakes everyone anyway. On a log that goes quiet immediately
//! after, it does not heal at all: the consumer waits for an append that has
//! already happened, forever. So every subscription ALSO re-reads on
//! [`SUBSCRIPTION_POLL_SECS`], which turns the worst case from unbounded into a
//! number.
//!
//! The wake-up is a [`tokio::sync::watch`] rather than a notify, and that
//! choice does real work: `changed()` is edge-triggered against a value the
//! receiver has not yet observed, so an append landing BETWEEN a read and the
//! wait is still pending when the wait begins. A primitive that only wakes
//! current waiters would drop exactly that one, and it is the likeliest race
//! here — the window is the width of a database round trip.
//!
//! # A slow consumer loses its subscription, not its connection
//!
//! Delivery is at-least-once and the cursor is what makes that safe: a consumer
//! that reconnects from its last delivered sequence misses nothing and may see
//! a repeat. When a consumer stops reading, the queue fills and the send blocks;
//! after [`SLOW_CONSUMER_TIMEOUT_SECS`] the subscription gives up and says so
//! with the cursor the consumer ACTUALLY received — not the one the kernel had
//! reached — so resuming from it has no gap. The connection stays open and its
//! other work carries on.

use std::sync::Arc;
use std::time::Duration;

use gwk_domain::ids::{RequestId, Seq};
use gwk_domain::port::EventStore;
use gwk_domain::protocol::{
    KernelErrorCode, SLOW_CONSUMER_TIMEOUT_SECS, SUBSCRIPTION_POLL_SECS, ServerControl,
};
use sqlx::PgPool;
use sqlx::postgres::PgListener;
use tokio::sync::{mpsc, watch};

use super::serve::fit_page;
use crate::store::{EVENT_CHANNEL, PgEventStore};

/// Events read per catch-up page.
///
/// A subscription that is far behind walks forward a page at a time rather than
/// asking for everything at once; the byte cut in [`fit_page`] then trims that
/// page to what one frame can carry.
const SUBSCRIPTION_BATCH_ROWS: usize = 256;

/// Turn database notifications into wake-ups, until the daemon stops.
///
/// The published value is a counter, not a watermark: subscriptions re-read
/// from their own cursors anyway, so all this has to carry is "something
/// changed". A counter is always different from the last one, which means a
/// receiver can never mistake two appends for one.
///
/// A listener that cannot connect or that dies is not fatal, and deliberately
/// so — it degrades a subscription from milliseconds to
/// [`SUBSCRIPTION_POLL_SECS`], and a kernel that refused to serve at all
/// because its notification channel was unavailable would be trading a slower
/// stream for no stream.
pub async fn watch_events(pool: PgPool, wake: Arc<watch::Sender<u64>>) {
    let mut listener = match PgListener::connect_with(&pool).await {
        Ok(listener) => listener,
        Err(_) => return,
    };
    if listener.listen(EVENT_CHANNEL).await.is_err() {
        return;
    }
    let mut tick = 0u64;
    while listener.recv().await.is_ok() {
        tick = tick.wrapping_add(1);
        // `send_replace` rather than `send`: with no subscriptions on any
        // connection there are no receivers, and a plain send would report that
        // as an error on every single append.
        wake.send_replace(tick);
    }
}

/// Why a subscription stopped, when it stopped for a reason worth reporting.
///
/// The reason travels as a code and nothing more, because `StreamClosed` carries
/// no message: what a client does about a stream that ended is resume from the
/// cursor, and the two codes that reach it — a slow consumer and a failed read —
/// are the whole of what it can branch on.
enum Ended {
    /// The consumer stopped reading and the queue stayed full.
    SlowConsumer,
    /// The log could not be read.
    Storage,
    /// The connection went away; nobody is left to tell.
    Gone,
}

/// Deliver from `cursor` forward until the consumer stops keeping up or the
/// connection ends.
///
/// `batches` is the queue the consumer drains and `responses` is the priority
/// one, which is why a `StreamClosed` can still be delivered to a consumer
/// whose batch queue is precisely the thing that is full.
pub async fn run(
    log: Arc<PgEventStore>,
    request_id: RequestId,
    mut cursor: Option<Seq>,
    batches: mpsc::Sender<ServerControl>,
    responses: mpsc::Sender<ServerControl>,
    mut wake: watch::Receiver<u64>,
) {
    let ended = loop {
        let events = match log.read_from(cursor, SUBSCRIPTION_BATCH_ROWS).await {
            Ok(events) => events,
            Err(_) => break Ended::Storage,
        };
        if events.is_empty() {
            // Nothing to send: wait for a notification or for the poll interval
            // to come round, whichever is first. `wake.changed()` was armed by
            // not having been observed since the last loop, so an append that
            // landed during the read above is already pending here.
            tokio::select! {
                changed = wake.changed() => {
                    if changed.is_err() {
                        // The listener task is gone. The poll still works, so
                        // this is slower, not broken.
                        tokio::time::sleep(Duration::from_secs(SUBSCRIPTION_POLL_SECS)).await;
                    }
                }
                () = tokio::time::sleep(Duration::from_secs(SUBSCRIPTION_POLL_SECS)) => {}
            }
            continue;
        }

        // The page is cut to what a frame can carry; whatever is left is picked
        // up on the next turn of the loop, immediately, because the cursor moved.
        let (events, _cut) = match fit_page(events) {
            Ok(page) => page,
            Err(_) => break Ended::Storage,
        };
        // `fit_page` always keeps its first item, so a non-empty read cannot cut
        // to nothing — and if it ever did, standing still with a cursor that
        // never moves would be the worse failure.
        let Some(last) = events.last().map(|e| e.global_sequence) else {
            break Ended::Storage;
        };

        let batch = ServerControl::EventBatch {
            request_id: request_id.clone(),
            events,
            cursor: last,
        };
        match tokio::time::timeout(
            Duration::from_secs(SLOW_CONSUMER_TIMEOUT_SECS),
            batches.send(batch),
        )
        .await
        {
            // Delivered — or at least queued, which is what at-least-once
            // means. The cursor advances only now, so a batch that never
            // reached the queue is re-read rather than skipped.
            Ok(Ok(())) => cursor = Some(last),
            Ok(Err(_)) => break Ended::Gone,
            Err(_) => break Ended::SlowConsumer,
        }
    };

    let code = match ended {
        // Nobody is left to tell, and the connection's own teardown is already
        // the message.
        Ended::Gone => return,
        Ended::SlowConsumer => KernelErrorCode::SlowConsumer,
        Ended::Storage => KernelErrorCode::Storage,
    };
    // `cursor` is what the consumer RECEIVED, which is the whole point: a
    // resume from it is gap-free even though the kernel had read further.
    let _ = responses
        .send(ServerControl::StreamClosed {
            request_id,
            code,
            last_cursor: cursor,
        })
        .await;
}
