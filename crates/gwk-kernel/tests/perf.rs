//! The phase performance envelope, measured.
//!
//! Seven numbers were accepted for this stage, and every one is a claim about a
//! real PostgreSQL 16 under a release build. This suite produces them, writes
//! them to a receipt, and fails naming every bound that missed — all of them,
//! not the first, because a run that stops at the first miss costs a second run
//! to learn the rest.
//!
//! The receipt carries one number that is not an accepted bound: the load the
//! latency phase actually offered. It rides in the same list so that a run which
//! failed to set up its own experiment still writes a receipt and still fails,
//! rather than aborting into silence — see [`latency_measurements`].
//!
//! ```text
//! GWK_TEST_ADMIN_DATABASE_URL=postgres://postgres@localhost:55432/postgres \
//!   cargo test --release -p gwk-kernel --test perf -- --ignored --nocapture
//! ```
//!
//! # One test, on purpose
//!
//! Every measurement here is a timing, and Rust's harness runs tests in
//! parallel: two of them against one PostgreSQL would each measure the other's
//! load. They are phases of a single case instead, so the ordering is the
//! source's and not a `--test-threads` flag the next person has to remember.
//!
//! # What the receipt is worth, and where
//!
//! The bounds are hardware numbers. On the operator's box they are asserted as
//! written, and that run is the phase evidence. A hosted CI runner is slower and
//! noisier, so it sets `GWK_PERF_SLACK` to a generous multiple and keeps the
//! receipt as an artifact: an order-of-magnitude collapse still reds the build,
//! runner variance does not, and nobody is tempted to repair a flaky gate by
//! quietly loosening the bound it was gating.

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use common::*;
use gwk_domain::checkpoint::CHECKPOINT_EVENT_INTERVAL;
use gwk_domain::command::KernelCommand;
use gwk_domain::ids::{AttentionItemId, EvidenceId, GateId, MessageId, Seq, TaskId, Timestamp};
use gwk_domain::port::EventStore;
use gwk_domain::protocol::{KernelResult, ServerControl};
use gwk_kernel::MAX_INFLIGHT_APPENDS;
use gwk_kernel::checkpoint;
use gwk_kernel::recover::Verdict;
use sqlx::PgPool;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio::time::MissedTickBehavior;

/// The offered load the latency bound is stated at.
const LATENCY_COMMANDS_PER_SECOND: u64 = 100;
/// Long enough that a p99 is over a hundred samples rather than twenty.
const LATENCY_SECONDS: u64 = 20;
/// "Sustain", as accepted: a minute, not a burst.
const THROUGHPUT_SECONDS: u64 = 60;
/// Concurrent submitters for the throughput phase. Appends serialize on the
/// writer row lock, so this is only deep enough to keep that lock from ever
/// being idle between two commands.
const THROUGHPUT_WORKERS: u64 = 16;
const RECOVERY_EVENTS: u64 = 100_000;
/// The envelope's "≤10,000 projection rows", which is also
/// [`CHECKPOINT_EVENT_INTERVAL`] — the interval a live kernel snapshots at, so
/// this is the checkpoint a clean shutdown actually leaves.
const VERIFY_ROWS: u64 = CHECKPOINT_EVENT_INTERVAL;
/// Samples for the subscription bound.
const DELIVERIES: u64 = 200;
const STORAGE_EVENTS: u64 = 1_000_000;
/// "1 KiB inline payload", carried as a title so it lands in the projection row
/// too — a real create would put it there.
const STORAGE_PAYLOAD_BYTES: usize = 1024;

const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// Which side of its bound a passing value falls on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum Direction {
    AtMost,
    AtLeast,
}

#[derive(Debug, serde::Serialize)]
struct Measurement {
    /// The envelope line this answers.
    name: &'static str,
    unit: &'static str,
    value: f64,
    bound: f64,
    direction: Direction,
    /// What was actually run, in one sentence. A number without its method is
    /// not evidence of anything.
    method: String,
    /// Against the bound as accepted, with no slack. This is the phase verdict.
    holds: bool,
    /// Whether a runner's slack may rescue this. False for the accepted hardware
    /// bounds, which is what slack exists for; true for a check on the harness's
    /// own validity, which no slack should ever pass.
    slack_exempt: bool,
}

impl Measurement {
    fn new(
        name: &'static str,
        unit: &'static str,
        value: f64,
        bound: f64,
        direction: Direction,
        method: String,
    ) -> Self {
        let holds = match direction {
            Direction::AtMost => value <= bound,
            Direction::AtLeast => value >= bound,
        };
        Self {
            name,
            unit,
            value,
            bound,
            direction,
            method,
            holds,
            slack_exempt: false,
        }
    }

    /// A check on the harness rather than on the kernel. Slack models a machine
    /// slower than the operator's box; this models an experiment that did not
    /// happen, and no multiple of a wrong run makes it a right one.
    fn harness(
        name: &'static str,
        unit: &'static str,
        value: f64,
        bound: f64,
        direction: Direction,
        method: String,
    ) -> Self {
        Self {
            slack_exempt: true,
            ..Self::new(name, unit, value, bound, direction, method)
        }
    }

    /// Whether it clears the bound after the runner's slack is applied. Equal to
    /// [`Self::holds`] at the default slack of 1, and always for a slack-exempt
    /// measurement.
    fn passes(&self, slack: f64) -> bool {
        let slack = if self.slack_exempt { 1.0 } else { slack };
        match self.direction {
            Direction::AtMost => self.value <= self.bound * slack,
            Direction::AtLeast => self.value * slack >= self.bound,
        }
    }

    fn line(&self) -> String {
        let sign = match self.direction {
            Direction::AtMost => "≤",
            Direction::AtLeast => "≥",
        };
        format!(
            "{:<28} {:>12.3} {} (bound {sign} {:.3}) {}",
            self.name,
            self.value,
            self.unit,
            self.bound,
            if self.holds { "ok" } else { "MISSED" }
        )
    }
}

#[derive(serde::Serialize)]
struct Receipt {
    /// What was measured. Set by CI from the commit under test; a receipt that
    /// cannot say which build it describes is not evidence.
    revision: Option<String>,
    postgres: String,
    slack: f64,
    measurements: Vec<Measurement>,
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires PostgreSQL, and spends minutes measuring"]
async fn the_phase_performance_envelope_holds() {
    // Refused rather than reported: a debug build misses every bound here by an
    // order of magnitude, and a receipt that does not say which profile produced
    // it is worse than no receipt.
    if cfg!(debug_assertions) {
        panic!("the envelope is release-mode evidence: re-run with --release");
    }
    let slack = std::env::var("GWK_PERF_SLACK")
        .ok()
        .and_then(|raw| raw.parse::<f64>().ok())
        .unwrap_or(1.0);
    let maintenance = maintenance_pool().await;

    let mut measurements = Vec::new();
    measurements.extend(command_latency(&maintenance).await);
    measurements.push(sustained_throughput(&maintenance).await);
    measurements.extend(cold_recovery(&maintenance).await);
    measurements.push(verification(&maintenance).await);
    measurements.push(subscription_delivery(&maintenance).await);
    measurements.push(storage_footprint(&maintenance).await);

    let receipt = Receipt {
        revision: std::env::var("GWK_PERF_REVISION").ok(),
        postgres: sqlx::query_scalar("SELECT current_setting('server_version')")
            .fetch_one(&maintenance)
            .await
            .expect("server version"),
        slack,
        measurements,
    };
    // Cargo's own scratch directory for this test target, because a test's
    // working directory is its CRATE root: a relative `target/…` would put the
    // receipt in `crates/gwk-kernel/target/`, which is not the workspace's and is
    // not ignored. CI passes an explicit path.
    let path = std::env::var("GWK_PERF_RECEIPT")
        .unwrap_or_else(|_| format!("{}/gwk-perf-receipt.json", env!("CARGO_TARGET_TMPDIR")));
    let json = serde_json::to_string_pretty(&receipt).expect("serialize the receipt");
    std::fs::write(&path, format!("{json}\n")).expect("write the receipt");

    for measurement in &receipt.measurements {
        println!("{}", measurement.line());
    }
    println!("receipt: {path}");

    let missed: Vec<&str> = receipt
        .measurements
        .iter()
        .filter(|m| !m.passes(slack))
        .map(|m| m.name)
        .collect();
    assert!(
        missed.is_empty(),
        "the envelope missed at slack {slack}: {missed:?} — see {path}"
    );
}

/// **At 100 mixed single-event commands/second: p95 ≤50 ms and p99 ≤200 ms.**
///
/// Offered load, not closed-loop: a tick every 10 ms whether or not the previous
/// command has answered, so a slow kernel shows up as queueing in the samples
/// rather than as a quietly reduced rate. Missed ticks are delayed rather than
/// made up in a burst — catching up would measure a load nobody offered.
///
/// Open-loop with one bound. The generator never offers more concurrency than the
/// store admits, because past [`MAX_INFLIGHT_APPENDS`] the kernel refuses — which
/// is CORRECT backpressure and not a latency sample, but arrives here as
/// "a command was not applied" and reads like a kernel defect. That is a claim
/// about the harness's own load, not about the kernel, so the harness owns it.
/// Half the store's admission leaves headroom for the rest of the process; a
/// machine genuinely too slow for the offered rate now stretches `offered` and is
/// reported by the rate assertion below, which says so in those words.
async fn command_latency(maintenance: &PgPool) -> Vec<Measurement> {
    let (name, store) = fresh_store(maintenance, "perf_latency", MAX_INFLIGHT_APPENDS).await;
    let store = Arc::new(store);
    let total = LATENCY_COMMANDS_PER_SECOND * LATENCY_SECONDS;

    let mut ticker = tokio::time::interval(Duration::from_micros(
        1_000_000 / LATENCY_COMMANDS_PER_SECOND,
    ));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut inflight = JoinSet::new();
    let offering = Arc::new(Semaphore::new(MAX_INFLIGHT_APPENDS / 2));
    let started = Instant::now();
    for n in 0..total {
        ticker.tick().await;
        let store = Arc::clone(&store);
        let permit = Arc::clone(&offering)
            .acquire_owned()
            .await
            .expect("the offering semaphore is never closed");
        inflight.spawn(async move {
            let _permit = permit;
            let command = mixed_command(n);
            let envelope = envelope(&format!("perf-lat-{n}"), &command);
            let at = Instant::now();
            let result = store.submit(&envelope).await;
            (at.elapsed(), result)
        });
    }
    let offered = started.elapsed();

    let mut millis = Vec::with_capacity(total as usize);
    while let Some(joined) = inflight.join_next().await {
        let (elapsed, result) = joined.expect("join a submit");
        // A refusal is not a slow success: an overload refusal would leave the
        // percentiles looking excellent while the kernel served nothing.
        assert!(
            matches!(result, KernelResult::CommandApplied { .. }),
            "a command under the offered load was not applied: {result:?}"
        );
        millis.push(elapsed.as_secs_f64() * 1_000.0);
    }
    millis.sort_by(f64::total_cmp);
    let rate = total as f64 / offered.as_secs_f64();
    drop_database(maintenance, &name).await;

    let method = format!(
        "{total} mixed single-event commands over five aggregate types, offered at \
         {rate:.1}/s for {LATENCY_SECONDS}s, every one applied"
    );
    latency_measurements(rate, &millis, method)
}

/// The latency phase's verdict, separated from its measurement so the decision is
/// testable without a database and a minute of wall clock.
///
/// **The offered rate is reported, not asserted.** An assertion here fires before
/// the receipt is written, so the run produces no artifact at all — and CI cannot
/// tell a runner that was too busy from a job that broke. That inverted the whole
/// two-runner design: a real latency regression wrote its receipt and was
/// tolerated as one miss beside a clean fresh run, while runner contention — the
/// exact noise the second runner exists to absorb — hard-failed the build.
///
/// It is exempt from `GWK_PERF_SLACK` because it is not a hardware bound. Slack
/// says "this runner is slower than the operator's box"; this says "the harness
/// did not run the experiment the percentiles claim to describe", and a generous
/// multiple would turn that into a pass over samples nobody should read.
///
/// When it misses, the percentiles are **withheld** rather than reported as
/// missed. They are not bad numbers, they are numbers from a different
/// experiment, and a receipt listing them as failed bounds would send the next
/// reader after a latency regression that never happened.
fn latency_measurements(rate: f64, millis: &[f64], method: String) -> Vec<Measurement> {
    let offered = Measurement::harness(
        "command_latency_offered_rate",
        "cmd/s",
        rate,
        LATENCY_COMMANDS_PER_SECOND as f64 * 0.95,
        Direction::AtLeast,
        method.clone(),
    );
    if !offered.holds {
        return vec![offered];
    }
    vec![
        offered,
        Measurement::new(
            "command_latency_p95",
            "ms",
            percentile(millis, 95),
            50.0,
            Direction::AtMost,
            method.clone(),
        ),
        Measurement::new(
            "command_latency_p99",
            "ms",
            percentile(millis, 99),
            200.0,
            Direction::AtMost,
            method,
        ),
    ]
}

/// **Sustain 1,000 events/second for 60 seconds.**
///
/// Through `submit`, which is the path that also writes the projections — the
/// port's raw `append` would post a larger number for a smaller job.
///
/// With a blob store attached, because a serving daemon has one and that is what
/// arms the checkpoint barrier: a minute at the accepted rate crosses
/// [`CHECKPOINT_EVENT_INTERVAL`] several times, and each crossing hashes every
/// projection row while holding the writer lock. Measuring without it would
/// report a throughput no deployment can reach.
async fn sustained_throughput(maintenance: &PgPool) -> Measurement {
    let (name, store) = fresh_store(maintenance, "perf_throughput", MAX_INFLIGHT_APPENDS).await;
    let (root, blobs) = blob_store(&store, "perf_throughput").await;
    let store = Arc::new(store.with_blobs(blobs));
    let applied = Arc::new(AtomicU64::new(0));
    let deadline = Instant::now() + Duration::from_secs(THROUGHPUT_SECONDS);

    let mut workers = JoinSet::new();
    for worker in 0..THROUGHPUT_WORKERS {
        let store = Arc::clone(&store);
        let applied = Arc::clone(&applied);
        workers.spawn(async move {
            let mut n = 0u64;
            while Instant::now() < deadline {
                let command = mixed_command(worker * 1_000_000 + n);
                let envelope = envelope(&format!("perf-thr-{worker}-{n}"), &command);
                match store.submit(&envelope).await {
                    KernelResult::CommandApplied { events, .. } => {
                        applied.fetch_add(events.len() as u64, Ordering::Relaxed);
                    }
                    other => panic!("throughput submit was refused: {other:?}"),
                }
                n += 1;
            }
        });
    }
    let started = Instant::now();
    while let Some(joined) = workers.join_next().await {
        joined.expect("join a worker");
    }
    let elapsed = started.elapsed();
    let events = applied.load(Ordering::Relaxed);
    // The log is the arbiter, not the counter: a command counted as applied that
    // is not in the log would be the only interesting result this phase could
    // produce.
    let logged = event_count(&store).await as u64;
    assert_eq!(events, logged, "counted appends disagree with the log");
    // The barrier has to have FIRED, or this measured a kernel without one.
    let snapshots: i64 = sqlx::query_scalar("SELECT count(*) FROM gwk_internal.checkpoint")
        .fetch_one(store.pool())
        .await
        .expect("count checkpoints");
    assert!(
        snapshots > 0,
        "the checkpoint barrier never crossed an interval: {events} events, {snapshots} snapshots"
    );
    drop_database(maintenance, &name).await;
    let _ = std::fs::remove_dir_all(&root);

    Measurement::new(
        "sustained_events_per_second",
        "events/s",
        events as f64 / elapsed.as_secs_f64(),
        1_000.0,
        Direction::AtLeast,
        format!(
            "{events} single-event commands through `submit` — events and projections in one \
             transaction — from {THROUGHPUT_WORKERS} concurrent clients over {:.1}s, with the \
             checkpoint barrier armed and {snapshots} snapshots taken inside the window",
            elapsed.as_secs_f64()
        ),
    )
}

/// **Replay ≥1,000 events/second; 100,000-event cold recovery ≤120 seconds.**
///
/// Cold means a log with empty projections, which is the only state replay runs
/// from — and the state a restore of the log alone leaves. The events are seeded
/// by INSERT because what is being measured is reading them back out.
async fn cold_recovery(maintenance: &PgPool) -> Vec<Measurement> {
    let (name, store) = fresh_store(maintenance, "perf_recovery", MAX_INFLIGHT_APPENDS).await;
    seed_command_events(store.pool(), RECOVERY_EVENTS, 0).await;

    let started = Instant::now();
    let report = store.recover().await.expect("recover");
    let elapsed = started.elapsed();
    let replayed = match report.verdict {
        Verdict::Replayed { events } => events,
        other => panic!("a log with empty projections must replay, got {other:?}"),
    };
    // Genesis is skipped by replay and activation carries no projection, so the
    // count is the seed plus the one epoch event that reaches `apply_event`.
    assert_eq!(replayed, RECOVERY_EVENTS + 1);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM gwk.task")
        .fetch_one(store.pool())
        .await
        .expect("count the rebuilt rows");
    assert_eq!(rows as u64, RECOVERY_EVENTS, "replay left rows unwritten");
    drop_database(maintenance, &name).await;

    let method = format!(
        "{RECOVERY_EVENTS} seeded `task_created` events replayed into empty projections by \
         `recover()`, verdict Replayed, {rows} rows written"
    );
    vec![
        Measurement::new(
            "replay_events_per_second",
            "events/s",
            replayed as f64 / elapsed.as_secs_f64(),
            1_000.0,
            Direction::AtLeast,
            method.clone(),
        ),
        Measurement::new(
            "cold_recovery_seconds",
            "s",
            elapsed.as_secs_f64(),
            120.0,
            Direction::AtMost,
            method,
        ),
    ]
}

/// **Verification adds ≤2 seconds over replay** (amendment 4): with a valid
/// checkpoint at the watermark, the hash comparison over ≤10,000 projection
/// rows.
///
/// The comparison IS the addition. Replay builds the rows and is already bounded
/// by the line above; what this phase times is the second `recover()`, which
/// replays nothing — it derives the live records, hashes them, reads the
/// checkpoint's records blob back and hashes that.
async fn verification(maintenance: &PgPool) -> Measurement {
    let (name, store) = fresh_store(maintenance, "perf_verify", MAX_INFLIGHT_APPENDS).await;
    let (root, blobs) = blob_store(&store, "perf_verify").await;
    let store = store.with_blobs(blobs);
    seed_command_events(store.pool(), VERIFY_ROWS, 0).await;

    // Cold first: the rows have to exist before anything can hash them.
    let built = store.recover().await.expect("build the projections");
    assert!(matches!(built.verdict, Verdict::Replayed { .. }));

    // A checkpoint AT the watermark — what a clean shutdown leaves, and the only
    // state `Verified` is reachable from.
    let watermark = store.watermark().await.expect("watermark").expect("a log");
    let mut tx = store.pool().begin().await.expect("begin the snapshot");
    let checkpoint = checkpoint::snapshot(
        &mut tx,
        store.blobs().expect("blobs"),
        watermark,
        &Timestamp::new("2026-07-29T00:00:00Z"),
    )
    .await
    .expect("snapshot");
    tx.commit().await.expect("commit the snapshot");
    assert_eq!(checkpoint.through_sequence, watermark);

    let started = Instant::now();
    let report = store.recover().await.expect("verify");
    let elapsed = started.elapsed();
    match report.verdict {
        Verdict::Verified { anchor } => assert_eq!(anchor, watermark),
        other => panic!("a checkpoint at the watermark must verify, got {other:?}"),
    }
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM gwk.task")
        .fetch_one(store.pool())
        .await
        .expect("count rows");
    assert_eq!(rows as u64, VERIFY_ROWS);
    drop_database(maintenance, &name).await;
    let _ = std::fs::remove_dir_all(&root);

    Measurement::new(
        "verification_seconds",
        "s",
        elapsed.as_secs_f64(),
        2.0,
        Direction::AtMost,
        format!(
            "`recover()` against a checkpoint at the watermark over {rows} projection rows: \
             derive, hash, read the records blob back, compare — verdict Verified, no replay"
        ),
    )
}

/// **Subscription delivery p95 ≤100 ms.**
///
/// Over the real socket, with the real notification listener, measured from the
/// moment the submitting client has its answer to the moment the event reaches a
/// DIFFERENT client's subscription. The append committed slightly before that
/// answer arrived, so every sample here is an overstatement — which is the right
/// direction for a bound.
async fn subscription_delivery(maintenance: &PgPool) -> Measurement {
    let (name, store) = fresh_store(maintenance, "perf_subscribe", MAX_INFLIGHT_APPENDS).await;
    let running = Running::open(store, "perf_subscribe").await;
    let mut watcher = running.client().await;
    let mut writer = running.client().await;

    // From the watermark, so the samples are live deliveries and not the
    // catch-up page a cursor of `None` would ask for first.
    let from = watermark_of(&mut watcher, "wm").await;
    match watcher.ask("sub", &subscribe_from(from)).await {
        KernelResult::Subscribed { cursor } => assert_eq!(cursor, Some(from)),
        other => panic!("{other:?}"),
    }

    let mut millis = Vec::with_capacity(DELIVERIES as usize);
    for n in 0..DELIVERIES {
        let command = mixed_command(n);
        let envelope = envelope(&format!("perf-sub-{n}"), &command);
        let body = serde_json::to_string(&envelope).expect("serialize the envelope");
        let applied = writer
            .ask(
                &format!("submit-{n}"),
                &format!(r#"{{"type":"submit_command","envelope":{body}}}"#),
            )
            .await;
        let at = Instant::now();
        let sequences = match applied {
            KernelResult::CommandApplied { events, .. } => events
                .iter()
                .map(|e| e.global_sequence)
                .collect::<BTreeSet<Seq>>(),
            other => panic!("submit {n} was refused: {other:?}"),
        };

        // Batches may coalesce or arrive with earlier events still in them, so
        // the sample ends when THIS command's sequence has landed.
        let mut outstanding = sequences;
        while !outstanding.is_empty() {
            match watcher.recv().await.expect("a batch") {
                ServerControl::EventBatch { events, .. } => {
                    for event in events {
                        outstanding.remove(&event.global_sequence);
                    }
                }
                other => panic!("{other:?}"),
            }
        }
        millis.push(at.elapsed().as_secs_f64() * 1_000.0);
    }
    millis.sort_by(f64::total_cmp);
    drop(watcher);
    drop(writer);
    running.close().await;
    drop_database(maintenance, &name).await;

    Measurement::new(
        "subscription_delivery_p95",
        "ms",
        percentile(&millis, 95),
        100.0,
        Direction::AtMost,
        format!(
            "{DELIVERIES} single-event commands submitted by one wire client, timed from its \
             response to the arrival of that event on a second client's subscription"
        ),
    )
}

/// **One million events with 1 KiB inline payload consume ≤5 GiB including
/// indexes and projections.**
///
/// Both sides are seeded by statement — the events, and the projection rows
/// derived from them (operator ruling, 2026-07-29). A replayed million costs
/// twenty minutes per run to produce rows of the same width: what is being
/// measured is bytes on disk, and a row does not get wider for having travelled
/// through `apply_event`. The receipt says so.
async fn storage_footprint(maintenance: &PgPool) -> Measurement {
    let (name, store) = fresh_store(maintenance, "perf_storage", MAX_INFLIGHT_APPENDS).await;
    seed_command_events(store.pool(), STORAGE_EVENTS, STORAGE_PAYLOAD_BYTES).await;
    let rows = seed_task_rows(store.pool()).await;
    assert_eq!(rows, STORAGE_EVENTS, "one projection row per create");

    // Heap plus every index plus TOAST, over both schemas: `pg_total_relation_size`
    // is the total for a table, so index relations must NOT also be summed.
    let bytes: String = sqlx::query_scalar(
        "SELECT coalesce(sum(pg_total_relation_size(c.oid)), 0)::text \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE n.nspname IN ('gwk', 'gwk_internal') AND c.relkind IN ('r', 'p', 'm')",
    )
    .fetch_one(store.pool())
    .await
    .expect("total relation size");
    let bytes: f64 = bytes.parse().expect("a byte count");
    // Guards the query as much as the kernel: a WHERE clause that matched
    // nothing would report a very comfortable zero.
    assert!(
        bytes > STORAGE_EVENTS as f64 * STORAGE_PAYLOAD_BYTES as f64,
        "{bytes} bytes is less than the payloads alone: the size query is measuring nothing"
    );
    drop_database(maintenance, &name).await;

    Measurement::new(
        "storage_gib_per_million",
        "GiB",
        bytes / GIB,
        5.0,
        Direction::AtMost,
        format!(
            "{STORAGE_EVENTS} events carrying a {STORAGE_PAYLOAD_BYTES}-byte inline title plus \
             the {rows} projection rows derived from them, both seeded by statement; \
             pg_total_relation_size over schemas gwk and gwk_internal — heap, indexes and TOAST"
        ),
    )
}

/// Five single-event commands on five aggregate types, rotating.
///
/// Mixed has to mean more than a different id: these land in five different
/// projection tables, so the number is not one INSERT plan measured a thousand
/// times.
fn mixed_command(n: u64) -> KernelCommand {
    match n % 5 {
        0 => KernelCommand::CreateTask {
            task_id: TaskId::new(format!("t-mix-{n}")),
            kind: Some("phase".into()),
            title: Some("measure the envelope".into()),
            spec_ref: None,
            project: Some(PROJECT.to_owned()),
            priority: Some(3),
            tracker_ref: None,
        },
        1 => KernelCommand::SendMessage {
            message_id: MessageId::new(format!("m-mix-{n}")),
            correlation_id: None,
            reply_to: None,
            sender: Some("orchestrator".into()),
            recipient: Some("engine-a".into()),
            channel: Some("dispatch".into()),
            kind: Some("brief".into()),
            payload: Some(serde_json::json!({ "n": n })),
            deadline: None,
        },
        2 => KernelCommand::RaiseAttention {
            attention_item_id: AttentionItemId::new(format!("att-mix-{n}")),
            kind: "risk_tag".to_owned(),
            summary: "measured".to_owned(),
            subject_ref: None,
            raised_by: None,
            priority: None,
        },
        3 => KernelCommand::RecordEvidence {
            evidence_id: EvidenceId::new(format!("ev-mix-{n}")),
            kind: "diff".to_owned(),
            r#ref: format!("blob://sha256-{n}"),
            digest: None,
            byte_size: None,
        },
        _ => KernelCommand::OpenGate {
            gate_id: GateId::new(format!("g-mix-{n}")),
            attempt_id: None,
            phase_ref: Some("4p-kernel".into()),
            kind: Some("review".into()),
            question: None,
            options: None,
        },
    }
}

/// Nearest-rank percentile over an ascending slice.
///
/// No interpolation: a p99 that is an average of two samples is a number no
/// request ever experienced, and at these sample counts the rank is what the
/// bound is about.
fn percentile(sorted: &[f64], pct: usize) -> f64 {
    assert!(!sorted.is_empty(), "no samples");
    let rank = (sorted.len() * pct).div_ceil(100).max(1) - 1;
    sorted[rank.min(sorted.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_percentile_is_a_sample_and_the_bound_is_read_off_the_direction() {
        let samples: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&samples, 95), 95.0);
        assert_eq!(percentile(&samples, 99), 99.0);
        assert_eq!(percentile(&samples, 100), 100.0);
        assert_eq!(percentile(&[7.0], 99), 7.0);

        let at_most = Measurement::new("l", "ms", 50.0, 50.0, Direction::AtMost, String::new());
        assert!(at_most.holds, "the bound is inclusive");
        assert!(!Measurement::new("l", "ms", 50.1, 50.0, Direction::AtMost, String::new()).holds);
        let slow = Measurement::new("l", "ms", 149.0, 50.0, Direction::AtMost, String::new());
        assert!(!slow.holds && slow.passes(3.0), "slack passes what missed");

        let at_least = Measurement::new(
            "t",
            "events/s",
            999.0,
            1_000.0,
            Direction::AtLeast,
            String::new(),
        );
        assert!(!at_least.holds);
        assert!(at_least.passes(1.01));
        assert!(
            Measurement::new(
                "t",
                "events/s",
                1_000.0,
                1_000.0,
                Direction::AtLeast,
                String::new()
            )
            .holds
        );
    }

    #[test]
    fn an_under_offered_run_withholds_its_percentiles_and_no_slack_rescues_it() {
        // Tenths of a millisecond, so a valid run clears both latency bounds and
        // the only thing under test is the offered-rate decision.
        let millis: Vec<f64> = (1..=100).map(|n| f64::from(n) / 10.0).collect();

        // 82.3/s against a 100/s target is the shape a contended hosted runner
        // produced on 2026-08-14, and the shape that used to abort before the
        // receipt was written.
        let missed = latency_measurements(82.3, &millis, String::new());
        assert_eq!(
            missed.len(),
            1,
            "percentiles from an under-offered run describe a different \
             experiment and must not reach the receipt"
        );
        assert_eq!(missed[0].name, "command_latency_offered_rate");
        assert!(!missed[0].holds, "82.3/s is not 100/s");
        // The one that matters: CI runs GWK_PERF_SLACK=3. Slack must not turn a
        // harness failure into a pass over samples nobody should read.
        assert!(
            !missed[0].passes(3.0),
            "slack rescued an invalid experiment"
        );
        assert!(!missed[0].passes(1_000.0));

        let clean = latency_measurements(100.0, &millis, String::new());
        assert_eq!(
            clean.len(),
            3,
            "a valid run reports the offered rate and both percentiles"
        );
        assert!(clean.iter().all(|m| m.holds));
        assert_eq!(clean[1].name, "command_latency_p95");
        assert_eq!(clean[2].name, "command_latency_p99");
        assert!(
            !clean[1].slack_exempt,
            "a hardware bound still takes the runner's slack"
        );

        // The floor is 95% of the target, and it is inclusive.
        assert!(latency_measurements(95.0, &millis, String::new())[0].holds);
        assert_eq!(latency_measurements(94.9, &millis, String::new()).len(), 1);
    }
}
