//! The normalization layer three unrelated protocols converge on.
//!
//! Three engines, three protocols: one real ACP client and two vendor wires
//! that share nothing with it or with each other. What they DO share is what
//! `docs/PARITY.md` requires of all three, and this module is that requirement
//! expressed as types instead of as prose.
//!
//! # Why the vocabulary is not invented here
//!
//! Every variant below is a row that already exists in `docs/PARITY.md`. Axis 1
//! names exactly four lifecycle facts — start, idle, error, end — and gives the
//! per-engine surface that carries each; axis 2 names exactly three states. A
//! richer enum would be this module's opinion rather than the contract's, and
//! the first engine that did not fit would have to argue with a type nobody
//! ratified. If a fifth fact is ever needed, `docs/PARITY.md` gains a column
//! first and this enum follows it.
//!
//! # What this is NOT
//!
//! It is not `agent_client_protocol`'s `Agent`. That name was carried in three
//! crate docs and one plan amendment as "the ACP SDK's role machinery ... this
//! crate implements the `Agent` role server-side over the vendor protocol", and
//! the claim could not be made true as written: `Agent` is a zero-sized marker
//! struct implementing the SDK's `Role` trait — a type-level tag for one end of
//! a JSON-RPC connection, with `Agent::Counterpart = Client`. There is nothing
//! to implement. Standing up the connection it tags would mean running an ACP
//! endpoint inside each vendor adapter and speaking JSON-RPC to ourselves,
//! which buys wire convergence and not the type convergence the parity harness
//! needed. The SDK dependency stays where a real ACP wire is spoken:
//! `gwk-adapter-opencode`, and nowhere else.
//!
//! # What it buys
//!
//! `gwk-parity` compared `&[String]` lifecycle tags — three adapters agreeing
//! on spelling. With [`LifecycleFact`] the agreement is checked by the compiler,
//! a misspelling stops being expressible, and a fourth engine becomes a
//! question of implementing [`EngineAdapter`] rather than of writing a fourth
//! protocol client and hoping its strings match.
//!
//! Deliberately outside the wire contract: nothing here derives `specta::Type`
//! and nothing is registered in `xtask`'s export registry. This is how the
//! host's own components agree with each other, not something a client parses.

use crate::ids::{EngineId, EngineSessionId};

/// One of the four lifecycle facts every adapter must observe.
///
/// `docs/PARITY.md` §Axis 1: "An adapter observes session start, idle, error,
/// and end through the engine's control surface." The surface differs per
/// engine — a `system`/`init` stream message, a `thread/started` notification,
/// a `session.created` bus event — and that difference is exactly what an
/// adapter exists to erase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LifecycleFact {
    Started,
    Idle,
    Errored,
    Ended,
}

impl LifecycleFact {
    /// The tag `gwk-parity` reported before these were types.
    ///
    /// Kept so a harness run still reads the same in a log and in
    /// `docs/PARITY.md`'s own prose. It is a rendering of the fact, never the
    /// carrier of it — the check compares variants.
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Started => "start",
            Self::Idle => "idle",
            Self::Errored => "error",
            Self::Ended => "end",
        }
    }
}

impl std::fmt::Display for LifecycleFact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.tag())
    }
}

/// The supervisor's belief about an engine.
///
/// `docs/PARITY.md` §Axis 2: "working, idle, or waiting on approval". Three
/// states and no fourth — an engine's own status vocabulary is richer (codex
/// has `notLoaded`/`systemError`, opencode has `retry`), and collapsing that
/// richness is the point rather than a loss. What a supervisor does with
/// `retry` and with `busy` is the same thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EngineStatus {
    Working,
    Idle,
    WaitingOnApproval,
}

impl EngineStatus {
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Idle => "idle",
            Self::WaitingOnApproval => "waiting_on_approval",
        }
    }
}

impl std::fmt::Display for EngineStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.tag())
    }
}

/// One fact an adapter extracted from its engine's control surface.
///
/// The session id is the ENGINE's own — a codex `threadId`, an opencode
/// `sessionID`, a claude `session_id` — carried rather than translated, because
/// the adapter is the only thing that can map it back and a caller that needs
/// to talk to the engine needs the engine's spelling.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineEvent {
    /// A point on the session's lifeline.
    Lifecycle {
        session: EngineSessionId,
        fact: LifecycleFact,
    },
    /// A push of the engine's own belief about itself.
    Status {
        session: EngineSessionId,
        status: EngineStatus,
    },
    /// The engine is blocked on a decision. The correlation id is the
    /// engine's, and answering is the host's job, not this layer's — an
    /// adapter that decided approvals would be a policy engine wearing an
    /// adapter's name.
    ApprovalAsked {
        session: EngineSessionId,
        request: String,
    },
    /// Spend the engine reported. Carried as the engine stated it; turning it
    /// into a ledger row is `KernelCommand::RecordCostEntry`'s job, and each
    /// adapter's `cost` module already owns that mapping.
    CostReported { session: EngineSessionId },
    /// Something real that this vocabulary does not name.
    ///
    /// Not an error and not dropped. Every engine's event set is open and far
    /// wider than four facts — opencode alone pushes file, tool, message-part
    /// and TUI events — so a caller can see the stream stayed alive and log
    /// what went unmodeled. A `Result` here would make "the engine said
    /// something we do not model" indistinguishable from "the engine said
    /// something malformed", and only the second is a fault.
    Unmodeled { tag: String },
}

impl EngineEvent {
    /// The lifecycle fact this event carries, if it carries one.
    ///
    /// The harness's axis-1 check is a fold over this: a status push is not a
    /// lifecycle fact however much it correlates with one.
    pub fn lifecycle(&self) -> Option<LifecycleFact> {
        match self {
            Self::Lifecycle { fact, .. } => Some(*fact),
            _ => None,
        }
    }

    /// The engine session this is about, if it is about one.
    ///
    /// `None` for [`EngineEvent::Unmodeled`] alone — an unparsed tag has no
    /// attribution to offer, and inventing one would be worse than admitting
    /// it. Every modeled variant is attributed by construction.
    pub fn session(&self) -> Option<&EngineSessionId> {
        match self {
            Self::Lifecycle { session, .. }
            | Self::Status { session, .. }
            | Self::ApprovalAsked { session, .. }
            | Self::CostReported { session } => Some(session),
            Self::Unmodeled { .. } => None,
        }
    }
}

/// What every engine adapter is, reduced to what they have in common.
///
/// `Raw` is an associated type rather than a shared input, because the inputs
/// genuinely do not converge: a codex JSON-RPC notification, an opencode SSE
/// bus frame and a claude stream-json message are three unrelated shapes, and a
/// trait that took `&serde_json::Value` would be pretending otherwise while
/// throwing away each adapter's own parsing. The convergence is on the OUTPUT,
/// which is the only place it was ever true.
///
/// Rendering is not here. Every adapter has a `spawn_tui` and all three are the
/// same two lines, but a trait method returning a `gwk_pty::Session` would put
/// the terminal engine into the contract crate, and the PTY layer is under a
/// clean-room gate this one is not. They stay free functions: identical is not
/// the same as shared.
pub trait EngineAdapter {
    /// The engine's own protocol frame, already parsed by the adapter.
    type Raw;

    /// Why a frame could not be normalized. Each adapter keeps its own, because
    /// a useful parse error names the protocol it failed in.
    type Error;

    /// The identity this adapter reports into the contract.
    fn engine_id(&self) -> EngineId;

    /// Normalize one protocol frame into the facts it carries.
    ///
    /// A `Vec` because the mapping is not one-to-one in either direction: a
    /// codex `turn/completed` is both an idle fact and a cost report, and a
    /// frame that carries no fact at all yields nothing rather than a variant
    /// meaning "nothing".
    fn normalize(&self, raw: Self::Raw) -> Result<Vec<EngineEvent>, Self::Error>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lifecycle_tags_are_the_four_docs_parity_names() {
        // The strings the harness used to compare, now derived from the type
        // rather than typed at each call site. If a variant is added without a
        // `docs/PARITY.md` column to justify it, this is where the mismatch
        // shows up as a decision rather than as a silently-passing new tag.
        let all = [
            LifecycleFact::Started,
            LifecycleFact::Idle,
            LifecycleFact::Errored,
            LifecycleFact::Ended,
        ];
        assert_eq!(
            all.map(LifecycleFact::tag),
            ["start", "idle", "error", "end"]
        );
    }

    #[test]
    fn only_a_lifecycle_event_reports_a_lifecycle_fact() {
        let session = EngineSessionId::new("s-1");
        let idle_fact = EngineEvent::Lifecycle {
            session: session.clone(),
            fact: LifecycleFact::Idle,
        };
        let idle_status = EngineEvent::Status {
            session: session.clone(),
            status: EngineStatus::Idle,
        };
        // The two correlate on every engine and are still different claims: one
        // says the session reached a point on its lifeline, the other says the
        // engine currently believes it is not working. The harness's axis 1
        // must not be satisfiable by a status push.
        assert_eq!(idle_fact.lifecycle(), Some(LifecycleFact::Idle));
        assert_eq!(idle_status.lifecycle(), None);
    }

    #[test]
    fn every_modeled_event_is_attributed_and_only_the_unmodeled_one_is_not() {
        let session = EngineSessionId::new("s-1");
        for event in [
            EngineEvent::Lifecycle {
                session: session.clone(),
                fact: LifecycleFact::Started,
            },
            EngineEvent::Status {
                session: session.clone(),
                status: EngineStatus::Working,
            },
            EngineEvent::ApprovalAsked {
                session: session.clone(),
                request: "req-1".to_owned(),
            },
            EngineEvent::CostReported {
                session: session.clone(),
            },
        ] {
            assert_eq!(event.session(), Some(&session), "{event:?}");
        }
        assert_eq!(
            EngineEvent::Unmodeled {
                tag: "file.edited".to_owned()
            }
            .session(),
            None
        );
    }
}
