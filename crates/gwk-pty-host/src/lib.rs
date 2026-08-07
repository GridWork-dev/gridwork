//! `gwk-pty-host`: the resident process that supervises PTY engine sessions —
//! spawn, pump loop, restart semantics, session registry, and detach/reattach
//! routing (CLEANROOM.md's own words for what this crate is gated for).
//!
//! # Shape of the crate
//!
//! Two halves, joined by the [`session`] task:
//!
//! - **The engine-facing half** — [`session`] supervises one child on a PTY
//!   through `gwk_pty::Session` (pump loop, recording, styled frames, restart
//!   semantics), [`wire`] converts the engine's typed output into the wire
//!   contract's `PtyFrame`/`PtyDelta` shapes, [`registry`] holds every live
//!   session by id and routes the verbs (spawn, input, resize, snapshot,
//!   attach, stop), and [`engines`] maps an engine name to its adapter's own
//!   spawn function.
//! - **The kernel-facing half** — command origination. Every command this
//!   host mints — an envelope with a minted id and an idempotency key,
//!   submitted over the kernel's Unix socket, its result classified into
//!   something a caller can act on — lives in [`envelope`],
//!   [`kernel_client`], and [`origination`], including the two command
//!   families with no builder anywhere else in the workspace ([`ingest`] for
//!   `IngestRecord`, [`dispatch_node`] for
//!   `RegisterDispatchNode`/`TransitionDispatchNode`). One normalized
//!   transcript record travels the whole path through
//!   [`origination::originate_record`].
//!
//! Building this crate now needs the same Zig 0.15.2 + pinned `ghostty`
//! toolchain as `gwk-pty` itself (`crates/gwk-pty/pins.env`,
//! `tools/pty-toolchain.sh`) — the engine dependency PR #43 deferred for
//! want of that toolchain is wired in here, and the `pty-host` CI job
//! materializes it the same way the `pty` job does.
//!
//! # The boundary that remains
//!
//! The kernel serves `PtyAttach`/`PtySnapshot` to consumers, but v1 of the
//! wire has no family a host can PUSH frames through — the kernel's own
//! serve loop records that its session registry "lands with the pin-advance
//! task that wires the PTY host in" (`crates/gwk-kernel/src/wire/serve.rs`),
//! and frame kind `0x02` is reserved-and-refused. Until that task, this
//! crate's [`registry`] is where attach, snapshot, and delta batches are
//! served FROM ([`registry::SessionRegistry::attach`] returns exactly the
//! catch-up-plus-live shape the wire contract describes); the hookup that
//! carries them across the socket is that follow-up's, not silently absent.
//! `docs/PARITY.md`'s axes 1, 2, and 4 (lifecycle, status, approval relay)
//! ride the same follow-up — they need the adapters' control halves, which
//! no 6′ verify demands of the host yet.
//!
//! # Clean-room scope
//!
//! This crate is under `CLEANROOM.md`'s second-review gate
//! (`.github/cleanroom-paths.txt`), by the `crates/gwk-pty` prefix and by its
//! own explicit row. The supervision code here drives the engine crate's
//! typed API and never parses a terminal-protocol byte itself — the parser,
//! and every fact about PTY semantics, stays in `gwk-pty` where its
//! `Derivation:` markers already live. Every file here carries rule 3's
//! declaration form stating what it does instead.

// Derivation: none — this file is crate-level documentation and module
// wiring only: no process is spawned, no byte is parsed, no session is
// supervised here.

#![doc(html_root_url = "https://docs.rs/gwk-pty-host")]

pub mod dispatch_node;
pub mod engines;
pub mod envelope;
pub mod ingest;
pub mod kernel_client;
pub mod logging;
pub mod origination;
pub mod registry;
pub mod session;
pub mod wire;
