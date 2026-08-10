//! The forwarding half of the attach hookup: what the registry serves
//! locally, published across the kernel socket.
//!
//! One task per session, each on its own outbound [`KernelClient`]
//! connection — the client asks one question at a time by design, and a
//! session's stream is exactly one conversation. The task attaches to its
//! own registry session like any consumer would, publishes the catch-up
//! snapshot, then forwards every live batch; on any break in continuity —
//! the kernel restarting, this connection refused as stale, the local
//! broadcast lagging — it drops the connection and starts over from a fresh
//! local attach, which is always sufficient: a snapshot reseed is the
//! contract's own answer to a gap on either side of the socket.
//!
//! Sessions are declared by the operator in [`SESSIONS_ENV`], because the
//! resident host serves this box and the box's owner decides what runs on
//! it. Routing spawn requests across the socket is the lifecycle follow-up
//! the crate root doc scopes to the adapters' control halves — not this
//! module's.
//!
//! Derivation: none — channel plumbing and typed client calls only; no
//! terminal byte is parsed and no process is supervised here (the registry
//! and session modules own those, and this module drives their public API).

use std::time::Duration;

use gwk_domain::ids::PtySessionId;
use gwk_domain::protocol::{FRAME_BODY_MAX_BYTES, KernelErrorCode};
use tokio::sync::watch;

use crate::kernel_client::{KernelClient, KernelClientError};
use crate::registry::Attacher;
use crate::session::{CatchUp, RestartPolicy, SessionConfig};

/// The operator's session declarations: comma-separated
/// `name=engine:COLSxROWS` entries, e.g.
/// `console=opencode:100x30,pair=claude:120x40`. Absent or empty means the
/// host runs no session, which is a legal resident state.
pub const SESSIONS_ENV: &str = "GWK_PTY_HOST_SESSIONS";

/// How long a publisher waits before reconnecting after any break — the
/// same order as the unit's own `RestartSec`, and for the same reason: fast
/// enough that a kernel restart is a blip, slow enough not to spin.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// Recording entries a declared session retains — the seq mint and the
/// replay substrate. Entries hold output chunks, so this bounds session
/// memory at roughly the cap times the read-buffer size.
const RECORDING_CAP: usize = 1024;

/// Delta batches a declared session retains for gapless local reattach,
/// matching the kernel hub's own window so neither side is the weak link.
const RETAINED_BATCHES: usize = 256;

/// Restarts a failed engine child gets before the session ends. The unit's
/// `Restart=on-failure` is the outer layer once the whole host is what
/// fails.
const RESTARTS: u32 = 5;

/// The pause between engine-child restarts.
const RESTART_DELAY: Duration = Duration::from_secs(2);

/// One operator-declared session: what to run and at what size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionDecl {
    pub id: PtySessionId,
    pub engine: String,
    pub cols: u16,
    pub rows: u16,
}

impl SessionDecl {
    /// The config every declared session runs under.
    pub fn config(&self) -> SessionConfig {
        SessionConfig {
            cols: self.cols,
            rows: self.rows,
            recording_cap: RECORDING_CAP,
            retained_batches: RETAINED_BATCHES,
            restart: RestartPolicy::OnFailure {
                max: RESTARTS,
                delay: RESTART_DELAY,
            },
        }
    }
}

/// Parse the operator's declarations, refusing the whole value on the first
/// malformed entry — a session the operator declared and the host silently
/// skipped would be the worst of both. Components are trimmed around the
/// separators, so `console = opencode : 100x30` does not mint a session id
/// with a trailing space nobody can type twice the same way.
pub fn parse_sessions(value: &str) -> Result<Vec<SessionDecl>, String> {
    let mut declared = Vec::new();
    for entry in value.split(',').map(str::trim).filter(|e| !e.is_empty()) {
        let (name, spec) = entry
            .split_once('=')
            .ok_or_else(|| format!("{entry:?} is not name=engine:COLSxROWS"))?;
        let (engine, size) = spec
            .split_once(':')
            .ok_or_else(|| format!("{entry:?} is missing its :COLSxROWS size"))?;
        let (cols, rows) = size
            .split_once('x')
            .ok_or_else(|| format!("{entry:?} is missing its COLSxROWS size"))?;
        let cols: u16 = cols
            .trim()
            .parse()
            .map_err(|_| format!("{entry:?} has a non-numeric column count"))?;
        let rows: u16 = rows
            .trim()
            .parse()
            .map_err(|_| format!("{entry:?} has a non-numeric row count"))?;
        let (name, engine) = (name.trim(), engine.trim());
        if name.is_empty() || engine.is_empty() || cols == 0 || rows == 0 {
            return Err(format!("{entry:?} has an empty name, engine, or size"));
        }
        if blank_grid_bytes(rows, cols) > PUBLISH_BYTE_BUDGET {
            // The kernel holds every snapshot publish to its budget, and a
            // grid whose BLANK form already exceeds it could never seed —
            // the publisher would be refused on every reconnect, forever.
            // Refusing the declaration here fails the unit loudly at start
            // instead.
            return Err(format!(
                "{entry:?} declares a screen too large to publish: a blank \
                 {rows}x{cols} frame serializes past the {PUBLISH_BYTE_BUDGET}-byte \
                 kernel budget"
            ));
        }
        let decl = SessionDecl {
            id: PtySessionId::new(name),
            engine: engine.to_owned(),
            cols,
            rows,
        };
        if declared.iter().any(|d: &SessionDecl| d.id == decl.id) {
            return Err(format!("{name:?} is declared twice"));
        }
        declared.push(decl);
    }
    Ok(declared)
}

/// The kernel's per-publish byte budget: half its frame cap, mirroring
/// `gwk-kernel`'s `wire::pty::PUBLISH_BYTE_BUDGET` (private there by
/// design — the wire's contract is the refusal, not the constant; if the
/// two ever drift, the kernel's `validation` refusal is fatal to the
/// session's publisher and loud in the journal, not a silent wedge).
const PUBLISH_BYTE_BUDGET: u64 = (FRAME_BODY_MAX_BYTES as u64) / 2;

/// The exact serialized size of an all-blank `rows`x`cols` frame, by the
/// same arithmetic as the kernel hub's own `blank_grid_bytes` under the
/// interned shape: one measured blank style, one measured `fill` run per
/// row (its `count` is `cols`, so the digits are right by construction),
/// plus JSON punctuation. O(rows), so every practical geometry declares
/// fine and the refusal below keeps its job only at the far edge.
fn blank_grid_bytes(rows: u16, cols: u16) -> u64 {
    let style = gwk_domain::frame::CellStyle {
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
    };
    let style = serde_json::to_vec(&style)
        .expect("a blank style serializes")
        .len() as u64;
    let fill = serde_json::to_vec(&gwk_domain::frame::PtyRun::Fill {
        style: 0,
        glyph: " ".to_owned(),
        count: u32::from(cols),
    })
    .expect("a fill run serializes")
    .len() as u64;
    // `{"styles":[` style `],"rows":[` rows x `[` fill `]` joined by commas
    // `]}` — 22 punctuation bytes outside the per-row term. `parse_sessions`
    // refused zero dimensions before this is ever asked.
    (style + 22).saturating_add(u64::from(rows).saturating_mul(fill + 3))
}

/// Forward one session to the kernel until it ends, or `stop` says so.
///
/// The loop's whole error strategy is one move: any failure — connect,
/// publish, refusal, local lag — drops the connection, waits, and starts
/// over from a fresh local attach. Session state is the durable truth; what
/// the kernel holds is a mirror this loop can always rebuild. The one
/// exception is a `validation` refusal: the kernel judged the VALUE, not
/// the moment, so the identical retry would be refused identically forever
/// — that ends the publisher loudly instead of wedging it on a 2s cadence.
pub async fn publish_session(
    attacher: Attacher,
    socket: std::path::PathBuf,
    id: PtySessionId,
    mut stop: watch::Receiver<bool>,
) {
    loop {
        if *stop.borrow() {
            return;
        }
        match publish_once(&attacher, &socket, &id, &mut stop).await {
            // The session itself ended (or a stop was asked): nothing left
            // to publish, and the registry's reap will report why.
            Outcome::SessionOver => return,
            Outcome::Fatal(why) => {
                tracing::error!(
                    session = %id, %why,
                    "the kernel refused this session's publish as invalid; \
                     giving up rather than retrying a deterministic refusal"
                );
                return;
            }
            Outcome::Retry(why) => {
                tracing::warn!(session = %id, %why, "publish interrupted; will reconnect");
            }
        }
        tokio::select! {
            () = tokio::time::sleep(RECONNECT_DELAY) => {}
            _ = stop.changed() => {}
        }
    }
}

enum Outcome {
    SessionOver,
    /// A refusal that retrying cannot change.
    Fatal(String),
    Retry(String),
}

/// Split a kernel-client failure by what a retry could do about it. A
/// `validation` refusal is the kernel judging the value itself; every other
/// failure — transport, staleness, an ownership race with a dying twin, a
/// full hub — is a moment a fresh connection and reseed can land the other
/// side of.
fn refusal_outcome(context: &str, error: &KernelClientError) -> Outcome {
    match error {
        KernelClientError::Refused {
            code: KernelErrorCode::Validation,
            ..
        } => Outcome::Fatal(format!("{context}: {error}")),
        _ => Outcome::Retry(format!("{context}: {error}")),
    }
}

/// One connection's worth of forwarding: attach locally, seed the kernel,
/// pump batches until something breaks.
async fn publish_once(
    attacher: &Attacher,
    socket: &std::path::Path,
    id: &PtySessionId,
    stop: &mut watch::Receiver<bool>,
) -> Outcome {
    let mut client = match KernelClient::connect(socket).await {
        Ok(client) => client,
        Err(e) => return Outcome::Retry(format!("connect: {e}")),
    };

    // A fresh attach — cursor `None` — is always snapshotted, which is
    // exactly the seed the kernel-side claim wants. The attach handle stands
    // alone on purpose: the round trip crosses the session's own thread, and
    // no shared lock should be held across one session's worst moment.
    let attached = match attacher.attach(None).await {
        Ok(attached) => attached,
        // The session's task has ended: nothing left to publish.
        Err(_) => return Outcome::SessionOver,
    };
    let CatchUp::Snapshotted(seed) = attached.catch_up else {
        // `catch_up` documents a `None` cursor as always snapshotted; this
        // arm is unreachable unless that contract changes underneath us.
        return Outcome::Retry("a fresh attach was not snapshotted".to_owned());
    };
    let mut live = attached.live;
    if let Err(e) = client.publish_snapshot(id, seed.seq, seed.frame).await {
        return refusal_outcome("seed", &e);
    }

    loop {
        let batch = tokio::select! {
            batch = live.recv() => batch,
            _ = stop.changed() => {
                // A last courtesy so consumers see a typed close now rather
                // than at the socket teardown; failure is fine — hanging up
                // retires just the same.
                let _ = client.retire(id).await;
                return Outcome::SessionOver;
            }
        };
        match batch {
            Ok(batch) => {
                if let Err(e) = client
                    .publish_deltas(id, batch.seq, batch.deltas.as_ref().clone())
                    .await
                {
                    // A stale-view refusal resyncs through the retry's fresh
                    // snapshot; a validation refusal never would.
                    return refusal_outcome("deltas", &e);
                }
            }
            // This publisher fell behind its own session's broadcast. The
            // batches it missed are unrecoverable on this subscription, so
            // resync the way the wire contract resyncs any gap: reconnect
            // and reseed from a fresh snapshot.
            Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                return Outcome::Retry(format!("lagged {missed} batches behind the session"));
            }
            // The session's task ended: tell the kernel now instead of
            // waiting for the socket to say it.
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                let _ = client.retire(id).await;
                return Outcome::SessionOver;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declarations_parse_whole_or_not_at_all() {
        let declared =
            parse_sessions("console=opencode:100x30, pair=claude:120x40").expect("parses");
        assert_eq!(declared.len(), 2);
        assert_eq!(declared[0].id, PtySessionId::new("console"));
        assert_eq!(declared[0].engine, "opencode");
        assert_eq!((declared[0].cols, declared[0].rows), (100, 30));
        assert_eq!(declared[1].id, PtySessionId::new("pair"));

        assert_eq!(parse_sessions("").expect("empty is no sessions"), vec![]);
        assert_eq!(parse_sessions("  ,  ").expect("blanks drop out"), vec![]);

        // Whitespace around the separators trims — a stray space must not
        // mint a session id nobody can type twice the same way.
        let spaced = parse_sessions(" console = opencode : 100x30 ").expect("trims");
        assert_eq!(spaced[0].id, PtySessionId::new("console"));
        assert_eq!(spaced[0].engine, "opencode");
        assert_eq!((spaced[0].cols, spaced[0].rows), (100, 30));

        // A screen whose blank form cannot fit the kernel's publish budget
        // is refused at declaration — the alternative is a publisher refused
        // identically on every reconnect, forever. Under the interned shape
        // the blank form is O(rows), so the refusal now lives at the far
        // edge: tens of thousands of rows, not a big-but-real screen.
        assert!(
            parse_sessions("big=opencode:100x50000").is_err(),
            "an unpublishable declared size should refuse"
        );
        // The spike's deciding geometry — 240x70 busted the budget alone
        // under the per-cell shape — declares fine now. That flip is the
        // element's whole point, pinned where the old refusal was.
        assert!(
            parse_sessions("wide=opencode:240x70").is_ok(),
            "a 240x70 screen seeds under the interned shape"
        );

        for bad in [
            "console",                      // no engine
            "console=opencode",             // no size
            "console=opencode:100",         // no rows
            "console=opencode:wide x tall", // non-numeric
            "console=opencode:0x30",        // zero cols
            "=opencode:100x30",             // empty name
            "console=:100x30",              // empty engine
            "a=sh:80x24,a=sh:80x24",        // duplicate id
        ] {
            assert!(parse_sessions(bad).is_err(), "{bad:?} should refuse");
        }
    }

    #[test]
    fn the_b4_floor_of_sixteen_declared_sessions_all_seed_at_declaration() {
        // B4's 12-16-session floor, every one at the spike's worst measured
        // geometry: each declaration passes the blank-form budget check that
        // refused even ONE such screen before the interning.
        let value = (0..16)
            .map(|n| format!("s{n}=opencode:240x70"))
            .collect::<Vec<_>>()
            .join(",");
        let declared = parse_sessions(&value).expect("all sixteen declare");
        assert_eq!(declared.len(), 16);
    }

    #[test]
    fn a_declared_session_config_carries_the_declared_size() {
        let decl = &parse_sessions("c=opencode:97x31").expect("parses")[0];
        let config = decl.config();
        assert_eq!((config.cols, config.rows), (97, 31));
        assert_eq!(config.retained_batches, RETAINED_BATCHES);
        assert!(matches!(config.restart, RestartPolicy::OnFailure { .. }));
    }
}
