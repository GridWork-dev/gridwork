//! A consumer detach is invisible to the child.
//!
//! Detaching is dropping an `Attached` — a broadcast subscription and
//! nothing else. The session task's side of the PTY stays open through
//! every attach and detach, so the events the terminal-interface chapter
//! ties its disconnect-and-close consequences to never occur here: nothing
//! is closed, nothing is sensed, no process ends. This test makes each
//! absent consequence a line the child would print if it arrived, and
//! drives a real `/bin/sh` through the registry to read them back.
//!
//! Derivation: POSIX-TERM §11.1.3 — the chapter moves a controlling
//! terminal's association only on named events: allocation by a session
//! leader, `setsid()`, the termination of the controlling process, and
//! "the close of the last file descriptor in the system" associated with
//! it (whose effect it leaves unspecified even then). That a detach —
//! which performs none of these — therefore leaves the association
//! untouched is this crate's reading of that enumeration, and it is the
//! reading the assertions below demonstrate.

use std::time::Duration;

use gwk_domain::ids::PtySessionId;
use gwk_pty_host::registry::SessionRegistry;
use gwk_pty_host::session::{RestartPolicy, SessionConfig, SessionExit, Snapshot, SpawnFn};

/// The child script. Every consequence a detach must not have is a line
/// this script would PRINT — the screen is the whole observation: a
/// delivered `SIGHUP` becomes `GOT-HUP` (trapped, so it is readable
/// instead of fatal), a zero-length read ends the loop and becomes
/// `SAW-EOF`, and a line that survived to the child comes back as
/// `ACK:<line>`. `done` ends the child by its own clean `exit 0`.
const SCRIPT: &str = "trap 'echo GOT-HUP' HUP; echo READY; \
    while read -r line; do \
    echo \"ACK:$line\"; \
    if [ \"$line\" = done ]; then exit 0; fi; \
    done; echo SAW-EOF";

fn shell() -> SpawnFn {
    Box::new(|cols, rows| {
        gwk_pty::Session::spawn(
            pty_process::Command::new("/bin/sh").arg("-c").arg(SCRIPT),
            cols,
            rows,
        )
    })
}

fn config() -> SessionConfig {
    SessionConfig {
        cols: 60,
        rows: 12,
        recording_cap: 1024,
        retained_batches: 1024,
        restart: RestartPolicy::Never,
    }
}

fn sid() -> PtySessionId {
    PtySessionId::new("detach")
}

fn rows_text(snapshot: &Snapshot) -> Vec<String> {
    snapshot
        .frame
        .cells
        .iter()
        .map(|row| {
            row.iter()
                .map(|cell| cell.glyph.as_str())
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

/// Poll the snapshot until `predicate` holds over the screen's rows —
/// bounded, so a broken session fails the test instead of hanging it.
async fn await_rows(
    registry: &SessionRegistry,
    predicate: impl Fn(&[String]) -> bool,
) -> Vec<String> {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(snapshot) = registry.snapshot(&sid()).await {
                let rows = rows_text(&snapshot);
                if predicate(&rows) {
                    return rows;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the awaited screen state never arrived")
}

#[tokio::test]
async fn a_detach_is_invisible_to_the_child() {
    let mut registry = SessionRegistry::new();
    registry
        .spawn(sid(), shell(), config())
        .await
        .expect("spawn");
    await_rows(&registry, |rows| rows.iter().any(|r| r == "READY")).await;

    // A consumer attaches and queues a line, then disconnects without
    // waiting for the child to have read it: the drop lands while the
    // line is still in flight from the consumer's point of view.
    let attached = registry.attach(&sid(), None).await.expect("attach");
    registry
        .input(&sid(), b"pending\n".to_vec())
        .await
        .expect("input");
    // Detaching IS dropping the subscription — `Attached`'s own contract.
    // Nothing else is released, and nothing is notified.
    drop(attached);

    // Derivation: POSIX-TERM §11.1.11 — "the last process to close a
    // terminal device file ... shall cause any input to be discarded".
    // The discard is bound to the LAST CLOSE, and a detach closes no
    // descriptor: the session task's side of the terminal stays open.
    // That the queued line must therefore survive the detach and reach
    // the child afterwards is this crate's reading of that binding.
    await_rows(&registry, |rows| rows.iter().any(|r| r == "ACK:pending")).await;

    // Input still flows after the detach — the attachment was never part
    // of the input path, and the child's reads keep being served.
    registry
        .input(&sid(), b"after\n".to_vec())
        .await
        .expect("input");

    // Derivation: POSIX-TERM §11.1.10 — the section makes its
    // consequences follow from one trigger: "if a modem disconnect is
    // detected by the terminal interface for a controlling terminal",
    // then "the SIGHUP signal shall be sent to the controlling process"
    // and "any subsequent read from the terminal device shall return the
    // value of zero, indicating end-of-file". A detach gives the
    // interface nothing to detect — no descriptor moved. The script
    // prints GOT-HUP for a delivered SIGHUP and SAW-EOF for a
    // zero-length read; both rows staying absent while lines keep being
    // acked is the demonstration that neither consequence occurred.
    let rows = await_rows(&registry, |rows| rows.iter().any(|r| r == "ACK:after")).await;
    assert!(
        !rows.iter().any(|r| r.contains("GOT-HUP")),
        "no SIGHUP may reach the child on a detach: {rows:?}"
    );
    assert!(
        !rows.iter().any(|r| r.contains("SAW-EOF")),
        "no read may return end-of-file on a detach: {rows:?}"
    );

    // The child ends on its own `exit 0`: a shell that had been
    // signal-terminated or EOF-starved could not reach `done` and exit
    // cleanly, so the exit status is the last word on the whole claim.
    registry
        .input(&sid(), b"done\n".to_vec())
        .await
        .expect("input");
    let reaped = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let reaped = registry.reap();
            if !reaped.is_empty() {
                return reaped;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the child never exited");
    let (_, exit) = &reaped[0];
    let SessionExit::Exited(status) = exit else {
        panic!("expected the child's own clean exit, got {exit:?}");
    };
    assert!(
        status.success(),
        "the child must end by its own `exit 0`, undisturbed: {status}"
    );
}
