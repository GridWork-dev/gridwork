//! The GridWork terminal console — the thin client half.
//!
//! **Four lenses exist.** The [`queue`] — the workday screen — the [`board`]
//! — the task/attempt DAG and the A2A message flow — and [`config`] — the
//! four-file git-backed reconciler — sit beside [`drilldown`], the hosted
//! session's styled-cell view.
//! Beneath the lenses sits the substrate they consume: [`theme`], the
//! one function that turns a resolved token into a renderer colour;
//! [`probe`], which measures what the drawing terminal does with the glyph
//! inventory; and [`input`], the session bracket, click hit-testing, and the
//! OSC 52 copy path. Beside them sits [`seven_act`]: the phase lifecycle as
//! client-side template data over the kernel's generic tasks and gates —
//! a convention the demonstration walks, not a domain object.
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

pub mod board;
pub mod config;
pub mod drilldown;
pub mod hall;
pub mod input;
pub mod probe;
pub mod queue;
pub mod replay;
pub mod seven_act;
pub mod theme;
