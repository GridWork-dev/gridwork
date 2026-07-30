//! Cursor paging, generatively: a walk sees the log exactly once.
//!
//! `read_from(cursor, limit)` is the only way anything reads the log — replay,
//! `gw log`, a subscription's catch-up, the wire's page request. All four are
//! loops over it, and all four are wrong in the same two ways if the page
//! boundary is: an event read twice becomes an event APPLIED twice, and an event
//! skipped becomes a projection that silently disagrees with its log.
//!
//! Neither failure needs an unusual event to appear — it needs an unusual
//! combination of log length and page size, which is what a generator is for and
//! what an example test picks one of. The exhaustive small cases matter most (a
//! limit of 1, a limit larger than the log, a log of one), and shrinking lands
//! on them.
//!
//! This is the PORT's contract, proved against the reference store. The
//! PostgreSQL implementation of the same contract is a `seq > $1 ORDER BY seq
//! LIMIT $2`, certified separately by the live suite — including the projection
//! walk under `COLLATE "C"`, where a locale collation really did drop rows at a
//! page boundary.

use gwk_cert::conformance::memory::InMemoryStore;
use gwk_domain::ids::Seq;
use gwk_domain::port::EventStore;
use proptest::prelude::*;

/// Walk the whole log one page at a time, exactly as every real reader does —
/// with the one guard a real reader does not have.
///
/// The cursor MUST advance. A boundary that hands back the event it was given
/// (`seq >= cursor` instead of `>`) makes this loop immortal rather than wrong,
/// and the first version of this test proved it: mutating the store that way
/// hung the run instead of failing it. In CI that is a timeout with no
/// diagnosis, so a stalled cursor is a named failure here.
async fn walk(store: &InMemoryStore, limit: usize) -> Result<Vec<u64>, String> {
    let mut seen = Vec::new();
    let mut cursor: Option<Seq> = None;
    loop {
        let page = store
            .read_from(cursor, limit)
            .await
            .map_err(|e| format!("read a page: {e}"))?;
        if page.is_empty() {
            return Ok(seen);
        }
        let last = page.last().map(|e| e.global_sequence);
        if last <= cursor {
            return Err(format!(
                "the cursor did not advance past {:?}: a page walk cannot terminate",
                cursor.map(Seq::value)
            ));
        }
        seen.extend(page.iter().map(|e| e.global_sequence.value()));
        cursor = last;
    }
}

proptest! {
    #[test]
    fn a_page_walk_sees_every_event_once_and_in_order(
        events in 0usize..64,
        limit in 1usize..80,
    ) {
        let store = InMemoryStore::new();
        store.seed(events);
        let walked = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a current-thread runtime")
            .block_on(walk(&store, limit));
        let seen = match walked {
            Ok(seen) => seen,
            Err(stalled) => return Err(TestCaseError::fail(stalled)),
        };

        // One assertion would do — equality against the full sequence covers
        // order, completeness and uniqueness at once — but three name which of
        // the three broke, and they are three different bugs.
        let expected: Vec<u64> = (1..=events as u64).collect();
        prop_assert_eq!(seen.len(), events, "a page walk lost or repeated events");
        let mut sorted = seen.clone();
        sorted.dedup();
        prop_assert_eq!(&sorted, &seen, "an event came back on two pages");
        prop_assert_eq!(&seen, &expected, "the walk was not the log in order");
    }
}

/// A limit of zero is the one page size that cannot make progress, and the
/// property above never generates it — so it is pinned here instead of being
/// quietly excluded.
///
/// The port does not refuse it: `take(0)` is an empty page, which a walking
/// reader reads as "the log ends here". That is the honest behaviour to record —
/// an empty page is the loop's only termination signal, and a caller asking for
/// nothing gets nothing rather than an error it has no arm for.
#[tokio::test]
async fn a_zero_limit_reads_nothing_rather_than_refusing() {
    let store = InMemoryStore::new();
    store.seed(3);
    assert!(
        store.read_from(None, 0).await.expect("read").is_empty(),
        "a zero limit must be an empty page, not an error and not a full one"
    );
    // And the log is still there for a caller that asks properly.
    assert_eq!(store.read_from(None, 3).await.expect("read").len(), 3);
}
