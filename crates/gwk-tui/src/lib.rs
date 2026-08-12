//! The GridWork terminal console — the thin client half.
//!
//! **Five lenses exist.** The [`queue`] — the workday screen — the [`board`]
//! — the task/attempt DAG and the A2A message flow — [`hall`] — the estate
//! frame, a deterministic spatial layout over caller-normalized facts (the
//! live projection/event join lives in [`estate`], the terminal loop in
//! [`runtime`]) — and [`config`] — the four-file git-backed reconciler — sit
//! beside [`drilldown`], the hosted session's styled-cell view.
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
//! `.github/cleanroom-paths.txt`). Its structural floor is in place: the
//! workspace/tab/split-pane model with create/navigate/resize/close, its
//! geometry solver, and the furniture painting that consumes [`chrome`]. The
//! wire half — session content in panes, detach/reattach routing — is not
//! built yet. The rest of this crate is lens code and is deliberately outside
//! the gate: the gate follows the risk, not the directory tree.
//!
//! [`chrome`] is the demonstration of that sentence. It themes the workspace's
//! own furniture and is named for it, and it still sits **outside** the gated
//! prefix — because rule 2 is a category test, not a directory test, and a
//! table mapping a chrome role to a ratified colour token supervises no
//! process and emits no terminal byte. Filing it under `src/workspace/`
//! would have gated it by path regardless of its category, which is exactly
//! the directory-tree reading rule 2 rejects.

pub mod board;
pub mod chrome;
pub mod config;
pub mod console;
pub mod drilldown;
pub mod estate;
pub mod hall;
pub mod input;
pub mod probe;
pub mod queue;
pub mod replay;
pub mod row;
pub mod runtime;
pub mod seven_act;
pub mod shell;
pub mod tables;
pub mod theme;
pub mod workspace;
