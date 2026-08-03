//! `gwk-pty-host`: the resident process that supervises PTY engine sessions —
//! spawn, pump loop, restart semantics, session registry, and detach/reattach
//! routing (CLEANROOM.md's own words for what this crate is gated for).
//!
//! **Skeleton only.** The runtime lands in a follow-up change; this crate
//! exists so the workspace, CI, and the clean-room gate treat it as a
//! first-class member from its first line rather than acquiring supervision
//! code before anyone decided whether it was covered. It carries no PTY
//! session logic yet and links nothing beyond the standard library.
//!
//! # Clean-room scope
//!
//! This crate is under `CLEANROOM.md`'s second-review gate
//! (`.github/cleanroom-paths.txt`), by the `crates/gwk-pty` prefix and by its
//! own explicit row. It asserts no terminal-protocol behavior yet, so rather
//! than invent a citation it carries rule 3's declaration form below. The
//! reason is the marker: a reviewer can check it as easily as a section number,
//! and the day this file gains a derived construct the declaration becomes
//! false in the one place someone editing it will be looking.

// Derivation: none — skeleton only: nothing here spawns a process, parses a byte,
// or supervises a session. The runtime and its citations land in the follow-up.

#![doc(html_root_url = "https://docs.rs/gwk-pty-host")]
