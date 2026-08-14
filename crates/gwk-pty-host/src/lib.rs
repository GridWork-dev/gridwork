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
//!   attach, stop), [`engines`] maps an engine name to its adapter's own
//!   spawn function, and [`publish`] carries each session's snapshot and
//!   delta batches to the kernel, one forwarding task per session.
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
//! # Hosted lifecycle
//!
//! The attach hookup is in: the wire's `pty_publish_snapshot` /
//! `pty_publish_deltas` / `pty_retire` family is how [`publish`] pushes
//! what [`registry::SessionRegistry::attach`] serves locally into the
//! kernel's own session registry (`crates/gwk-kernel/src/wire/pty.rs`),
//! and `PtyAttach`/`PtySnapshot` answer consumers from there. The raw fallback
//! is published beside that primary render-state path: JSON headers carry
//! correlation and kind `0x02` carries the byte-exact snapshot/output payload.
//! Input, resize, and stop now return across the same bounded owner connection,
//! addressed to one exact session generation. Sessions start either from the
//! operator's environment declaration ([`publish::SESSIONS_ENV`]) or from a
//! name-only request against the kernel's durable template catalog; executable
//! command, cwd, and environment data never ride a start request, and a catalog
//! child inherits only the environment map that catalog declared. Resident grid
//! allocations are bounded before spawn or resize, replaced start-manager routes
//! are rechecked before delivery, delivery retries are command-id deduplicated,
//! and ended sessions and publishers are periodically reaped.
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

pub mod control;
pub mod dispatch_node;
pub mod engines;
pub mod envelope;
pub mod ingest;
pub mod kernel_client;
pub mod logging;
pub mod origination;
pub mod publish;
pub mod registry;
pub mod session;
pub mod wire;
