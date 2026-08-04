//! `gwk-pty-host`: the resident process that supervises PTY engine sessions —
//! spawn, pump loop, restart semantics, session registry, and detach/reattach
//! routing (CLEANROOM.md's own words for what this crate is gated for).
//!
//! # Scope of this change
//!
//! This change lands the KERNEL-facing half: command origination. Every
//! command this host mints — an envelope with a minted id and an
//! idempotency key, submitted over the kernel's Unix socket, its result
//! classified into something a caller can act on — lives here, including
//! the two command families with no existing builder anywhere else in the
//! workspace ([`ingest`] for `IngestRecord`, [`dispatch_node`] for
//! `RegisterDispatchNode`/`TransitionDispatchNode`), both reachable from one
//! normalized transcript record via [`ingest::originate`].
//!
//! **PTY session supervision itself — spawning an engine, pumping its
//! output, session registry, restart semantics, detach/reattach routing —
//! is NOT wired into this change**, and that is a boundary this crate
//! records rather than blurs. Every crate that could supply it
//! (`gwk-pty`, and `gwk-adapter-claude`/`gwk-adapter-codex`/
//! `gwk-adapter-opencode`, all three of which depend on `gwk-pty`) needs
//! Zig 0.15.2 plus a pinned `ghostty` checkout to BUILD at all
//! (`crates/gwk-pty/pins.env`, `tools/pty-toolchain.sh`) — a toolchain this
//! change's execution environment does not carry, so a Cargo dependency on
//! any of the four would make `cargo test -p gwk-pty-host` fail to compile
//! here regardless of what this crate's own code does. Nothing in this
//! crate names or imports any of the four; the command-origination surface
//! below is deliberately engine-agnostic (a `TranscriptRecord` is JSON plus
//! an [`gwk_domain::IngestionKind`], never an adapter's own typed event
//! enum) so wiring in a real session registry later is additive rather than
//! a rewrite of what already works. `docs/PARITY.md`'s axis 3 (transcript
//! ingestion) is what this crate answers for; axes 1, 2, and 4 (lifecycle,
//! status, approval relay) all need a live session and stay with that
//! follow-up.
//!
//! # Clean-room scope
//!
//! This crate is under `CLEANROOM.md`'s second-review gate
//! (`.github/cleanroom-paths.txt`), by the `crates/gwk-pty` prefix and by its
//! own explicit row. Nothing in this change parses a terminal-protocol byte,
//! spawns a process, or supervises a session — it originates kernel commands
//! from already-normalized values — so every file here carries rule 3's
//! declaration form rather than a citation. The day PTY supervision lands,
//! the files that actually touch terminal bytes earn real `Derivation:`
//! markers of their own; this one stays honest about carrying none.

// Derivation: none — this file is crate-level documentation and module
// wiring only: no process is spawned, no byte is parsed, no session is
// supervised here.

#![doc(html_root_url = "https://docs.rs/gwk-pty-host")]

pub mod dispatch_node;
pub mod envelope;
pub mod ingest;
pub mod kernel_client;
pub mod logging;
pub mod origination;
