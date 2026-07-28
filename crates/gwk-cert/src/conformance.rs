//! The storage conformance suite: trait-generic checks any backend crate
//! instantiates against its own [`EventStore`] implementation.
//!
//! Each check panics with a description on violation (they are tests). Backends
//! run them under their own async runtime; [`memory::InMemoryStore`] is the
//! reference implementation this crate certifies itself against.
//!
//! Skeleton scope (the Contract stage): append/CAS/ordering, fencing, cursor
//! recovery, deterministic rebuild, watermark. Named-but-backend-level cases —
//! retention tombstones and kill-9 crash recovery — land with the first real
//! backend (they need a process boundary an in-memory store cannot fake).

use gwk_domain::envelope::{Actor, EventEnvelope, Origin};
use gwk_domain::ids::{AggregateId, EventId, ProjectId, Seq, Timestamp};
use gwk_domain::port::{AppendError, EventStore, MAX_READ_LIMIT};

/// A minimal valid envelope for conformance traffic. `global_sequence` and
/// `appended_at` carry sentinel values the store MUST overwrite.
pub fn fixture_event(aggregate_id: &str, aggregate_version: u32) -> EventEnvelope {
    EventEnvelope {
        event_id: EventId::new(format!("evt-{aggregate_id}-{aggregate_version}")),
        project_id: ProjectId::new("conformance"),
        aggregate_type: "task".into(),
        aggregate_id: AggregateId::new(aggregate_id),
        aggregate_version,
        event_type: "conformance_tick".into(),
        schema_version: gwk_domain::envelope::ENVELOPE_SCHEMA_VERSION,
        global_sequence: Seq::new(u64::MAX), // sentinel: must be overwritten
        occurred_at: Timestamp::new("2026-01-01T00:00:00Z"),
        appended_at: Timestamp::new("9999-01-01T00:00:00Z"), // sentinel
        actor: Actor {
            kind: "conformance".into(),
            id: None,
        },
        origin: Origin {
            system: "gwk-cert".into(),
            r#ref: None,
        },
        causation_id: None,
        correlation_id: None,
        idempotency_key: None,
        payload: serde_json::json!({}),
        payload_ref: None,
    }
}

/// Appends get strictly increasing, store-assigned sequences; client sentinel
/// values for `global_sequence`/`appended_at` are overwritten.
pub async fn check_append_assigns_commit_order<S: EventStore>(store: &S) {
    let a = store
        .append(0, None, vec![fixture_event("agg-a", 1)])
        .await
        .expect("first append");
    let b = store
        .append(0, None, vec![fixture_event("agg-b", 1)])
        .await
        .expect("second append");
    let seq_a = a[0].global_sequence;
    let seq_b = b[0].global_sequence;
    assert!(seq_a.value() < seq_b.value(), "sequence not increasing");
    assert_ne!(
        seq_a.value(),
        u64::MAX,
        "store kept the client sentinel seq"
    );
    assert_ne!(
        a[0].appended_at.as_str(),
        "9999-01-01T00:00:00Z",
        "store kept the client sentinel appended_at"
    );
}

/// The CAS refusal: appending at a version that already advanced conflicts,
/// and reports the actual version.
pub async fn check_expected_version_conflict<S: EventStore>(store: &S) {
    store
        .append(0, None, vec![fixture_event("agg", 1)])
        .await
        .expect("seed append");
    let err = store
        .append(0, None, vec![fixture_event("agg", 1)])
        .await
        .expect_err("stale append must conflict");
    assert_eq!(
        err,
        AppendError::VersionConflict {
            actual: 1,
            expected: 0
        }
    );
}

/// CAS refusal and recovery, sequentially: a second writer appending at a
/// stale `expected_version` is refused with the actual version, and recovers
/// by re-reading that version — never by blind retry. A genuinely concurrent
/// race (simultaneous appends under a real runtime) is a backend-side test;
/// this check pins the refusal/recovery contract, not the race itself.
pub async fn check_cas_refusal_and_recovery<S: EventStore>(store: &S) {
    store
        .append(0, None, vec![fixture_event("agg", 1)])
        .await
        .expect("writer A wins round 1");
    let loser = store
        .append(0, None, vec![fixture_event("agg", 1)])
        .await
        .expect_err("writer B must lose round 1");
    let AppendError::VersionConflict { actual, .. } = loser else {
        panic!("expected VersionConflict, got {loser:?}");
    };
    store
        .append(actual, None, vec![fixture_event("agg", actual + 1)])
        .await
        .expect("writer B wins after re-reading the actual version");
}

/// Fencing: after a re-grant, the old token is refused everywhere — and so
/// is an append that presents no token at all. Once the store has granted a
/// fence, the token is mandatory; omission must not bypass the check.
pub async fn check_fencing<S: EventStore>(store: &S) {
    let old = store.grant_fence().await.expect("first grant");
    store
        .append(0, Some(old), vec![fixture_event("agg", 1)])
        .await
        .expect("current fence accepted");
    let new = store.grant_fence().await.expect("second grant");
    assert!(old.value() < new.value(), "fence tokens must increase");
    let err = store
        .append(1, Some(old), vec![fixture_event("agg", 2)])
        .await
        .expect_err("stale fence must be refused");
    assert!(matches!(err, AppendError::Fenced { .. }), "got {err:?}");
    let err = store
        .append(1, None, vec![fixture_event("agg", 2)])
        .await
        .expect_err("omitted fence must be refused once one is granted");
    assert!(matches!(err, AppendError::Fenced { .. }), "got {err:?}");
    store
        .append(1, Some(new), vec![fixture_event("agg", 2)])
        .await
        .expect("current fence still works");
}

/// Wakeup loss is survivable: a consumer that missed every notification
/// recovers the full suffix by re-reading from its durable cursor.
pub async fn check_cursor_recovery<S: EventStore>(store: &S) {
    for i in 1..=5u32 {
        store
            .append(i - 1, None, vec![fixture_event("agg", i)])
            .await
            .expect("append");
    }
    let first_two = store.read_from(None, 2).await.expect("read page 1");
    assert_eq!(first_two.len(), 2);
    let cursor = first_two[1].global_sequence;
    // The consumer now "sleeps" through three more wakeups. Re-read: nothing lost.
    let rest = store.read_from(Some(cursor), 100).await.expect("read rest");
    assert_eq!(rest.len(), 3, "cursor recovery lost events");
    let mut all: Vec<u64> = first_two
        .iter()
        .chain(rest.iter())
        .map(|e| e.global_sequence.value())
        .collect();
    let sorted = {
        let mut s = all.clone();
        s.sort_unstable();
        s
    };
    assert_eq!(all.len(), 5);
    assert_eq!(all, sorted, "read order must be ascending");
    all.dedup();
    assert_eq!(all.len(), 5, "duplicate sequences across pages");
}

/// Reading the same committed log twice folds to the same result.
pub async fn check_deterministic_rebuild<S: EventStore>(store: &S) {
    for i in 1..=4u32 {
        store
            .append(i - 1, None, vec![fixture_event("agg", i)])
            .await
            .expect("append");
    }
    let once: Vec<(String, u32, u64)> = read_all(store).await;
    let twice: Vec<(String, u32, u64)> = read_all(store).await;
    assert_eq!(once, twice, "rebuild is not deterministic");
}

/// The watermark equals the last committed sequence.
pub async fn check_watermark<S: EventStore>(store: &S) {
    assert_eq!(
        store.watermark().await.expect("empty watermark"),
        None,
        "empty store must have no watermark"
    );
    store
        .append(0, None, vec![fixture_event("agg", 1)])
        .await
        .expect("append");
    let last = store
        .append(1, None, vec![fixture_event("agg", 2)])
        .await
        .expect("append")[0]
        .global_sequence;
    assert_eq!(
        store.watermark().await.expect("watermark"),
        Some(last),
        "watermark must equal the last committed sequence"
    );
}

/// The read clamp: `port.rs` documents [`MAX_READ_LIMIT`] as a ceiling a
/// conforming store MUST honour. A `read_from` with an enormous `limit` must
/// not error and must return at most that many events — no caller can demand an
/// unbounded page.
pub async fn check_read_limit_is_clamped<S: EventStore>(store: &S) {
    for i in 1..=8u32 {
        store
            .append(i - 1, None, vec![fixture_event("agg", i)])
            .await
            .expect("append");
    }
    // A `limit` below the page size is honoured EXACTLY — a store that ignored
    // `limit` would return all 8 here. (The prior assertion, `len() <= 65536`,
    // was vacuous: 8 is always <= MAX_READ_LIMIT.)
    let short = store.read_from(None, 3).await.expect("read");
    assert_eq!(
        short.len(),
        3,
        "read_from must honour a limit below the page size"
    );
    // An enormous `limit` must not error and must clamp to MAX_READ_LIMIT — with
    // 8 stored events that means all 8 come back, never more.
    let huge = store
        .read_from(None, usize::MAX)
        .await
        .expect("a huge limit must not error");
    assert_eq!(huge.len(), 8, "a huge limit returns all available events");
    assert!(
        huge.len() <= MAX_READ_LIMIT,
        "read_from must clamp to MAX_READ_LIMIT"
    );
}

async fn read_all<S: EventStore>(store: &S) -> Vec<(String, u32, u64)> {
    store
        .read_from(None, usize::MAX)
        .await
        .expect("read_all")
        .into_iter()
        .map(|e| {
            (
                e.aggregate_id.as_str().to_string(),
                e.aggregate_version,
                e.global_sequence.value(),
            )
        })
        .collect()
}

/// Run every check, each against a fresh store.
pub async fn run_all<S: EventStore, F: Fn() -> S>(fresh: F) {
    check_append_assigns_commit_order(&fresh()).await;
    check_expected_version_conflict(&fresh()).await;
    check_cas_refusal_and_recovery(&fresh()).await;
    check_fencing(&fresh()).await;
    check_cursor_recovery(&fresh()).await;
    check_deterministic_rebuild(&fresh()).await;
    check_watermark(&fresh()).await;
    check_read_limit_is_clamped(&fresh()).await;
}

pub mod memory {
    //! The in-memory reference [`EventStore`] — the implementation this crate
    //! certifies itself against, and a fixture backend for kernel tests.

    use std::sync::Mutex;

    use gwk_domain::envelope::EventEnvelope;
    use gwk_domain::ids::{FenceToken, Seq, Timestamp};
    use gwk_domain::port::{AppendError, EventStore, MAX_READ_LIMIT, StorageError};

    #[derive(Default)]
    struct Inner {
        events: Vec<EventEnvelope>,
        next_seq: u64,
        fence: u64,
    }

    /// Single-process reference store. Deterministic: `appended_at` derives
    /// from the assigned sequence, not the clock.
    #[derive(Default)]
    pub struct InMemoryStore {
        inner: Mutex<Inner>,
    }

    impl InMemoryStore {
        pub fn new() -> Self {
            Self::default()
        }
    }

    impl EventStore for InMemoryStore {
        async fn append(
            &self,
            expected_version: u32,
            fence: Option<FenceToken>,
            mut events: Vec<EventEnvelope>,
        ) -> Result<Vec<EventEnvelope>, AppendError> {
            let mut inner = self
                .inner
                .lock()
                .map_err(|e| AppendError::Storage(e.to_string()))?;
            // Once a fence has been granted, presenting the current token is
            // mandatory — omission is refused, not skipped, or a deposed
            // writer could keep writing by simply dropping its token.
            let presented = fence.unwrap_or(FenceToken::new(0));
            if presented.value() != inner.fence {
                return Err(AppendError::Fenced {
                    presented,
                    current: FenceToken::new(inner.fence),
                });
            }
            let Some(first) = events.first() else {
                return Err(AppendError::MalformedBatch("empty batch".into()));
            };
            let agg = (first.aggregate_type.clone(), first.aggregate_id.clone());
            for (i, event) in events.iter().enumerate() {
                if (event.aggregate_type.clone(), event.aggregate_id.clone()) != agg {
                    return Err(AppendError::MalformedBatch("mixed aggregates".into()));
                }
                let wanted = expected_version + 1 + i as u32;
                if event.aggregate_version != wanted {
                    return Err(AppendError::MalformedBatch(format!(
                        "non-contiguous version: got {}, want {wanted}",
                        event.aggregate_version
                    )));
                }
            }
            let actual = inner
                .events
                .iter()
                .filter(|e| (e.aggregate_type.clone(), e.aggregate_id.clone()) == agg)
                .map(|e| e.aggregate_version)
                .max()
                .unwrap_or(0);
            if actual != expected_version {
                return Err(AppendError::VersionConflict {
                    actual,
                    expected: expected_version,
                });
            }
            for event in &mut events {
                inner.next_seq += 1;
                event.global_sequence = Seq::new(inner.next_seq);
                // Deterministic assigned timestamp: derived from the sequence.
                event.appended_at = Timestamp::new(format!("seq:{}", inner.next_seq));
            }
            inner.events.extend(events.iter().cloned());
            Ok(events)
        }

        async fn read_from(
            &self,
            cursor: Option<Seq>,
            limit: usize,
        ) -> Result<Vec<EventEnvelope>, StorageError> {
            let inner = self.inner.lock().map_err(|e| StorageError(e.to_string()))?;
            let after = cursor.map(|s| s.value()).unwrap_or(0);
            Ok(inner
                .events
                .iter()
                .filter(|e| e.global_sequence.value() > after)
                .take(limit.min(MAX_READ_LIMIT))
                .cloned()
                .collect())
        }

        async fn watermark(&self) -> Result<Option<Seq>, StorageError> {
            let inner = self.inner.lock().map_err(|e| StorageError(e.to_string()))?;
            Ok(inner.events.last().map(|e| e.global_sequence))
        }

        async fn grant_fence(&self) -> Result<FenceToken, StorageError> {
            let mut inner = self.inner.lock().map_err(|e| StorageError(e.to_string()))?;
            inner.fence += 1;
            Ok(FenceToken::new(inner.fence))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::memory::InMemoryStore;

    /// Minimal executor for futures that never really pend (the in-memory
    /// store resolves on first poll). Real backends run the suite under their
    /// own runtime instead.
    fn block_on<F: Future>(fut: F) -> F::Output {
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        let mut fut = std::pin::pin!(fut);
        loop {
            match fut.as_mut().poll(&mut cx) {
                std::task::Poll::Ready(v) => return v,
                std::task::Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    #[test]
    fn reference_store_passes_the_full_suite() {
        block_on(super::run_all(InMemoryStore::new));
    }
}
