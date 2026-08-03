//! The GridWork terminal console — the thin client half.
//!
//! **This crate is a skeleton.** It exports nothing yet. The lenses that will
//! live here — the Queue, the Board, the config surface, the session
//! drill-down — are ordered behind a design gate this commit deliberately sits
//! in front of, so the crate lands as its manifest, its module boundary, and
//! its dependency posture, and nothing else. Painting anything here now would
//! be applying an idiom that has not been ratified.
//!
//! # What the crate is
//!
//! A client. It talks to the kernel over the UDS and consumes engine frames as
//! **wire data**: the PTY sessions and the engine adapters live server-side in
//! a separate host crate, and this one never links `gwk-pty`. That is what
//! makes attach-from-anywhere fall out rather than needing to be built — a
//! console that owned its own PTY could only ever show the sessions on the box
//! it happens to be running on.
//!
//! The invariant is asserted in CI (`cargo tree -p gwk-tui` carries no
//! `gwk-pty` or `libghostty`) with a positive control, because an invariant
//! nothing checks is a sentence in a doc comment.
//!
//! # `src/workspace/`
//!
//! The multiplexer work — panes, layout, detach/reattach routing — lands under
//! `src/workspace`, which is under the clean-room gate (`CLEANROOM.md` rule 2,
//! `.github/cleanroom-paths.txt`). Nothing in that module exists yet. The rest
//! of this crate is lens code and is deliberately outside the gate: the gate
//! follows the risk, not the directory tree.
