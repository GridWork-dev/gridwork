//! The wire-v2 Context READ grammar, and the one thing the handshake does
//! NOT carry.
//!
//! The write half — the ten lifecycle facts, [`RecordContextFact`], and
//! [`ContextEventPayload`] — lives in [`gwk_domain::context_event`] and is
//! re-exported from this crate's root. It moved there because `gwk-kernel`
//! must deserialize those facts to project them and cannot depend on an
//! unpublished crate; the grammar is unchanged by the move.
//!
//! **Nothing here is live.** [`gwk_domain::ProtocolVersion::V2`] is *known* to
//! the version grammar and *refused* by the kernel, and every type in this
//! module is a published shape rather than an accepted request. That staging is
//! the point: a grammar reviewed and frozen before anything speaks it is a
//! grammar whose first speaker cannot quietly renegotiate it.
//!
//! ## The two axes, and the one that does not move
//!
//! `ProtocolVersion` is the WIRE grammar. `gwk_domain::CONTRACT_VERSION` is the
//! DOMAIN contract — entity, event, and command shapes. They are separate axes
//! and this change moves exactly one of them (ruling R10 / fork F9). Adding a
//! known major is not an entity/event/command shape change, so
//! `CONTRACT_VERSION` stays `1` through the eventual cutover and bumps on its
//! own merits when a shape actually changes. A reflexive lockstep bump would
//! teach every future reader that the two always move together, which is the
//! belief R10 exists to prevent.
//!
//! ## Nothing Context-specific rides the hello
//!
//! Ruling R9 / fork F8. The handshake shape stays major/minor/capabilities/
//! client, and only the major flips. There is deliberately no `context_capable`
//! field and no `context` capability name, because ADR-0032 treats *mandatory*
//! and *capability* as opposites: a v2 major makes Context mandatory, full
//! stop. There is no optional v1 Context mode, no translator, no proxy, and no
//! dual stack — a peer either speaks a major that includes Context or it does
//! not connect. See [`CONTEXT_MANDATORY_FROM`].
//!
//! Hello is also per-CONNECTION while Context resolution is per-ATTEMPT, so a
//! connection-scoped flag would be answering a question at the wrong lifetime
//! even if the grammar wanted one.
//!
//! ## CTX-12 — attribution is provenance, not authorization
//!
//! Re-disclosed here because 8A is the phase required to state it, and because
//! this module is where it becomes structural rather than a promise.
//!
//! Same-EUID remains the entire authentication boundary for the kernel socket.
//! Context lifecycle events inherit that verbatim from threat 10; nothing in
//! this grammar narrows it and nothing here should be read as doing so. What
//! this grammar adds is one Context-scoped control: **a client cannot supply
//! source attribution.** [`RecordContextFact`] carries exactly one field and it
//! is the fact. [`ContextAttribution`] appears only on
//! [`ContextEventPayload`] — the kernel-side shape — and the compiler derives
//! it by re-reading its own resolved manifest rather than by trusting anything
//! a caller said about itself.
//!
//! The asymmetry is the mitigation, so it is asserted rather than described —
//! and asserted against the GENERATED surface rather than a list. The unit test
//! here walks a hand-written sample of each fact, which is a fast local check
//! and was, on its own, a guard with a hole the size of the next variant: an
//! eleventh fact carrying an `actor` and mapped onto an existing event name
//! passed it, because the sample list still held ten entries. The binding check
//! is `inspect_context_attribution` in `xtask`, which reads the generated
//! TypeScript — every variant and every field, by construction — and fails if
//! any client-submittable type declares an attribution-shaped property.
//!
//! ## Events ride the existing envelope
//!
//! There is no closed `KernelEvent` sum type in this system and this module
//! does not invent one. [`gwk_domain::EventEnvelope`] carries open bounded
//! `aggregate_type` / `event_type` strings over a generic JSON payload, so the
//! ten D4 lifecycle events are ten new `event_type` values under three new
//! `aggregate_type` values. [`ContextEventName`] and [`ContextAggregate`] are
//! closed enums over exactly those strings — closed on this side, open on the
//! envelope's, which is what lets the log accept them without a contract
//! change while every GridWork reader still branches exhaustively.

use gwk_domain::{AttemptId, ContextRunId, ContextWireError, Digest, ManifestId, ProtocolVersion};

use crate::ContextStage;

// ============================================================
// Handshake
// ============================================================

/// The major from which Context is MANDATORY.
///
/// Not "the major that supports Context" — there is no supported/unsupported
/// axis here. Below this major Context does not exist on the wire at all; at
/// and above it, every participant speaks it. That is the whole negotiation,
/// and it is why the hello has no Context field to negotiate with (R9).
pub const CONTEXT_MANDATORY_FROM: ProtocolVersion = ProtocolVersion::V2;

// ============================================================
// Bounds
// ============================================================

/// Maximum rows one projection read may ask for.
pub const CONTEXT_QUERY_LIMIT_MAX: u32 = 1_000;
/// Maximum edge hops a graph read may traverse.
pub const CONTEXT_GRAPH_DEPTH_MAX: u32 = 16;
/// Maximum stages one comparison may ask for.
///
/// `ContextStage::ALL` has five members, so anything past five is redundant by
/// construction and zero asks a comparison to compare nothing. Without this the
/// field was bounded only by the 4 MiB frame, in a module whose header claims
/// every count on this wire is bounded.
pub const CONTEXT_COMPARE_STAGE_MAX_COUNT: usize = 5;

// ============================================================
// Bounded scalars
// ============================================================

/// A bounded row limit for a projection read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct QueryLimit(u32);

impl QueryLimit {
    pub fn new(value: u32) -> Result<Self, ContextWireError> {
        if value == 0 || value > CONTEXT_QUERY_LIMIT_MAX {
            return Err(ContextWireError::QueryLimitOutOfRange);
        }
        Ok(Self(value))
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

impl<'de> serde::Deserialize<'de> for QueryLimit {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(u32::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

// A validated count on the wire.
impl specta::Type for QueryLimit {
    fn definition(types: &mut specta::Types) -> specta::datatype::DataType {
        <u32 as specta::Type>::definition(types)
    }
}

/// A bounded hop count for a graph read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct GraphDepth(u32);

impl GraphDepth {
    pub fn new(value: u32) -> Result<Self, ContextWireError> {
        if value == 0 || value > CONTEXT_GRAPH_DEPTH_MAX {
            return Err(ContextWireError::GraphDepthOutOfRange);
        }
        Ok(Self(value))
    }

    pub fn get(self) -> u32 {
        self.0
    }
}

impl<'de> serde::Deserialize<'de> for GraphDepth {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(u32::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

// A validated hop count on the wire.
impl specta::Type for GraphDepth {
    fn definition(types: &mut specta::Types) -> specta::datatype::DataType {
        <u32 as specta::Type>::definition(types)
    }
}

// ============================================================
// Reads
// ============================================================

/// Which supplement of a manifest a read wants.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    specta::Type,
)]
#[serde(rename_all = "snake_case")]
pub enum SupplementKind {
    Release,
    Observation,
    Finalization,
}

/// How a read names the manifest it wants.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(tag = "by", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManifestSelector {
    Id { manifest_id: ManifestId },
    Attempt { attempt_id: AttemptId },
}

/// What an Explain read is asking about.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(tag = "subject", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExplainSubject {
    /// Why this source did or did not participate.
    Source { digest: Digest },
    /// Why the precedence resolution landed where it did.
    Precedence,
    /// Why the manifest carries the participation set it carries.
    Participation,
}

/// One side of a comparison.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(tag = "of", rename_all = "snake_case", deny_unknown_fields)]
pub enum CompareSubject {
    Manifest { manifest_id: ManifestId },
    Run { run_id: ContextRunId },
}

/// The v2 Context read grammar.
///
/// Eight reads, matching ADR-0032's three projections plus the four record
/// reads and Compare. These are published shapes, not live handlers: 8A
/// publishes the grammar, and the projections behind it are later work.
///
/// Explain and Compare are powered by immutable projections linked to source
/// commits and CAS objects. Neither recomputes a historical manifest from
/// current files — a recomputed manifest answers a question about today and is
/// presented as an answer about the past, which is worse than no answer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(tag = "query", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContextQuery {
    /// One resolved manifest.
    Manifest { select: ManifestSelector },
    /// Supplements of one manifest, of one kind.
    Supplement {
        manifest_id: ManifestId,
        kind: SupplementKind,
        limit: QueryLimit,
    },
    /// One source as the manifest saw it, by content digest.
    Source {
        manifest_id: ManifestId,
        digest: Digest,
    },
    /// Sources, revisions, manifests, releases, observations, verifier
    /// receipts, and candidates — the provenance projection.
    ProvenanceGraph { root: ManifestId, depth: GraphDepth },
    /// Skills, memories, notes, concepts, symbols, files, and their citation
    /// and semantic relationships.
    SemanticGraph { root: Digest, depth: GraphDepth },
    /// Tasks, attempts, sessions, messages, tools, evidence, interventions,
    /// outcomes, and costs — execution causality, kept separate from the two
    /// above so nothing flattens into one ambiguous graph.
    ExecutionDag {
        run_id: ContextRunId,
        depth: GraphDepth,
    },
    /// Why sources participated, or did not.
    Explain {
        manifest_id: ManifestId,
        subject: ExplainSubject,
    },
    /// Declared, resolved, released, observed and final states across two
    /// subjects, at the stages asked for.
    Compare {
        left: CompareSubject,
        right: CompareSubject,
        stages: Vec<ContextStage>,
    },
}

impl ContextQuery {
    /// Reject a comparison of something with itself.
    ///
    /// Not a deserialization-time check: both sides are individually legal, and
    /// the rule is about their relationship. A self-comparison is always an
    /// empty diff, so answering it costs a projection read to learn nothing.
    pub fn validate(&self) -> Result<(), ContextWireError> {
        let Self::Compare {
            left,
            right,
            stages,
        } = self
        else {
            return Ok(());
        };
        if left == right {
            return Err(ContextWireError::CompareSubjectsIdentical);
        }
        if stages.is_empty() || stages.len() > CONTEXT_COMPARE_STAGE_MAX_COUNT {
            return Err(ContextWireError::CompareStagesOutOfRange);
        }
        // Duplicates are their own refusal rather than folded into the bound.
        // Five copies of one stage is inside the count and still asks the same
        // question five times, which a projection would answer five times.
        let mut seen = Vec::with_capacity(stages.len());
        for stage in stages {
            if seen.contains(&stage) {
                return Err(ContextWireError::CompareStagesRepeated);
            }
            seen.push(stage);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use gwk_domain::RecordContextFact;

    use super::*;

    fn manifest_id() -> ManifestId {
        ManifestId::parse("manifest-1").expect("valid id")
    }

    fn run_id() -> ContextRunId {
        ContextRunId::parse("run-1").expect("valid id")
    }

    #[test]
    fn bounded_scalars_refuse_zero_and_overflow() {
        assert_eq!(
            QueryLimit::new(0),
            Err(ContextWireError::QueryLimitOutOfRange)
        );
        assert_eq!(
            QueryLimit::new(CONTEXT_QUERY_LIMIT_MAX + 1),
            Err(ContextWireError::QueryLimitOutOfRange)
        );
        assert_eq!(
            QueryLimit::new(CONTEXT_QUERY_LIMIT_MAX).map(QueryLimit::get),
            Ok(CONTEXT_QUERY_LIMIT_MAX)
        );

        assert_eq!(
            GraphDepth::new(0),
            Err(ContextWireError::GraphDepthOutOfRange)
        );
        assert_eq!(
            GraphDepth::new(CONTEXT_GRAPH_DEPTH_MAX + 1),
            Err(ContextWireError::GraphDepthOutOfRange)
        );
    }
    #[test]
    fn a_bound_is_enforced_on_the_way_in_not_just_at_construction() {
        // The newtypes derive Serialize but hand-write Deserialize precisely so
        // an out-of-range value arriving from the wire is refused. A derived
        // Deserialize would accept it and this test is what notices.
        assert!(serde_json::from_str::<QueryLimit>("0").is_err());
        assert!(
            serde_json::from_str::<QueryLimit>(&(CONTEXT_QUERY_LIMIT_MAX + 1).to_string()).is_err()
        );
        assert!(serde_json::from_str::<GraphDepth>("0").is_err());
    }
    #[test]
    fn unknown_fields_are_refused_across_the_grammar() {
        // deny_unknown_fields on every wire shape: an unknown field is either
        // version skew or a caller inventing contract, and both should be loud.
        assert!(
            serde_json::from_str::<RecordContextFact>(
                r#"{"fact":{"fact":"run_opened","run_id":"r","manifest_id":"m","release_id":"x","opened_at":"2026-08-14T00:00:00Z"},"actor":"me"}"#
            )
            .is_err(),
            "an actor field must not deserialize onto a command"
        );
        assert!(
            serde_json::from_str::<ContextQuery>(
                r#"{"query":"manifest","select":{"by":"id","manifest_id":"m"},"limit":5}"#
            )
            .is_err(),
            "an unknown field must not deserialize onto a query"
        );

        // The positive controls. Both assertions above are `is_err`, so if
        // either shape stopped deserializing for an unrelated reason the test
        // would pass having proved nothing.
        serde_json::from_str::<RecordContextFact>(
            r#"{"fact":{"fact":"run_opened","run_id":"r","manifest_id":"m","release_id":"x","opened_at":"2026-08-14T00:00:00Z"}}"#,
        )
        .expect("the same command without the stray field must parse");
        serde_json::from_str::<ContextQuery>(
            r#"{"query":"manifest","select":{"by":"id","manifest_id":"m"}}"#,
        )
        .expect("the same query without the stray field must parse");

        // And one level down: a stray field inside the selector, not just
        // beside it. A guard that only reads the outermost object is one
        // nesting level from useless.
        assert!(
            serde_json::from_str::<ContextQuery>(
                r#"{"query":"manifest","select":{"by":"id","manifest_id":"m","actor":"me"}}"#
            )
            .is_err(),
            "an unknown field nested in a selector must not deserialize"
        );
    }

    #[test]
    fn compare_refuses_two_names_for_one_subject() {
        let same = ContextQuery::Compare {
            left: CompareSubject::Manifest {
                manifest_id: manifest_id(),
            },
            right: CompareSubject::Manifest {
                manifest_id: manifest_id(),
            },
            stages: ContextStage::ALL.to_vec(),
        };
        assert_eq!(
            same.validate(),
            Err(ContextWireError::CompareSubjectsIdentical)
        );

        let distinct = ContextQuery::Compare {
            left: CompareSubject::Manifest {
                manifest_id: manifest_id(),
            },
            right: CompareSubject::Run { run_id: run_id() },
            stages: ContextStage::ALL.to_vec(),
        };
        assert_eq!(distinct.validate(), Ok(()));
    }

    #[test]
    fn context_becomes_mandatory_at_v2_and_the_contract_number_does_not_move() {
        // R9: the flip is on the version axis alone. R10: the contract axis
        // stays where it is. Both in one assertion, because the failure mode is
        // someone moving them together.
        assert_eq!(CONTEXT_MANDATORY_FROM, ProtocolVersion::V2);
        assert_ne!(CONTEXT_MANDATORY_FROM, ProtocolVersion::V1);
        assert_eq!(gwk_domain::CONTRACT_VERSION, 1);
    }

    #[test]
    fn the_read_grammar_is_eight_reads_and_the_count_is_pinned() {
        // The module doc says eight. Nothing asserted it, so "eight" was a
        // claim in prose next to a grammar that could have grown a ninth.
        let reads = [
            r#"{"query":"manifest","select":{"by":"id","manifest_id":"m"}}"#,
            r#"{"query":"supplement","manifest_id":"m","kind":"release","limit":5}"#,
            &format!(
                r#"{{"query":"source","manifest_id":"m","digest":"sha256:{}"}}"#,
                "a".repeat(64)
            ),
            r#"{"query":"provenance_graph","root":"m","depth":2}"#,
            &format!(
                r#"{{"query":"semantic_graph","root":"sha256:{}","depth":2}}"#,
                "a".repeat(64)
            ),
            r#"{"query":"execution_dag","run_id":"r","depth":2}"#,
            r#"{"query":"explain","manifest_id":"m","subject":{"subject":"precedence"}}"#,
            r#"{"query":"compare","left":{"of":"manifest","manifest_id":"m"},"right":{"of":"run","run_id":"r"},"stages":["resolved"]}"#,
        ];
        assert_eq!(reads.len(), 8, "the read grammar is eight reads");

        let mut decoded = Vec::new();
        for raw in reads {
            decoded.push(
                serde_json::from_str::<ContextQuery>(raw)
                    .unwrap_or_else(|e| panic!("{raw} did not decode: {e}")),
            );
        }
        // Every one of them a DISTINCT variant: eight documents that all decoded
        // to the same read would satisfy the count and prove nothing.
        for (i, left) in decoded.iter().enumerate() {
            for right in decoded.iter().skip(i + 1) {
                assert_ne!(
                    std::mem::discriminant(left),
                    std::mem::discriminant(right),
                    "two reads decoded to the same variant"
                );
            }
        }
    }

    #[test]
    fn a_comparison_must_name_stages_and_must_not_repeat_them() {
        let subjects = || {
            (
                CompareSubject::Manifest {
                    manifest_id: manifest_id(),
                },
                CompareSubject::Run { run_id: run_id() },
            )
        };
        let compare = |stages: Vec<ContextStage>| {
            let (left, right) = subjects();
            ContextQuery::Compare {
                left,
                right,
                stages,
            }
        };

        assert_eq!(
            compare(Vec::new()).validate(),
            Err(ContextWireError::CompareStagesOutOfRange)
        );
        assert_eq!(
            compare(vec![
                ContextStage::Resolved;
                CONTEXT_COMPARE_STAGE_MAX_COUNT + 1
            ])
            .validate(),
            Err(ContextWireError::CompareStagesOutOfRange)
        );
        assert_eq!(
            compare(vec![ContextStage::Resolved, ContextStage::Resolved]).validate(),
            Err(ContextWireError::CompareStagesRepeated)
        );
        // The positive control, and the bound's own edge: all five distinct
        // stages is the largest legal comparison.
        assert_eq!(compare(ContextStage::ALL.to_vec()).validate(), Ok(()));
        assert_eq!(ContextStage::ALL.len(), CONTEXT_COMPARE_STAGE_MAX_COUNT);
    }
}
