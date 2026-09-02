//! Immutable Context truth records.
//!
//! One resolved manifest is the attempt-scoped artifact the compiler emits and
//! the independent verifier checks. Release, observation, and finalization are
//! separate append-only supplements because their cardinalities differ: one
//! release, zero or more ordered observations, and one finalization. None of
//! these records carries a stage discriminator; its concrete type is its stage.
//!
//! The records intentionally contain bounded metadata, counts, digests, and
//! evidence references only. Reconstructable rendered or prompt bytes belong in
//! the encrypted content-addressed store and are linked by digest later.

use crate::ids::{AttemptId, ByteCount, EvidenceId, Timestamp};

use crate::context_digest::Digest;
use crate::context_participation::Participation;

/// Maximum UTF-8 bytes in any Context truth-record identifier.
pub const CONTEXT_ID_MAX_BYTES: usize = 128;
/// Maximum evidence references carried inline by one truth record.
pub const CONTEXT_EVIDENCE_MAX_COUNT: usize = 64;
/// Maximum candidate participation rows carried by one resolved manifest.
pub const CONTEXT_PARTICIPATION_MAX_COUNT: usize = 4_096;
/// Maximum UTF-8 bytes in a participation's human-facing detail.
pub const CONTEXT_PARTICIPATION_DETAIL_MAX_BYTES: usize = 1_024;
/// Maximum value for small count fields serialized as JSON numbers.
pub const CONTEXT_RECORD_COUNT_MAX: u32 = 65_535;

/// Why bounded Context metadata failed validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruthRecordError {
    EmptyId,
    IdTooLong,
    InvalidIdCharacter,
    TooManyEvidenceRefs,
    InvalidEvidenceRef,
    TooManyParticipations,
    InvalidParticipation,
    ParticipationDetailTooLong,
    CountTooLarge,
    ObservationIndexOutOfRange,
}

impl std::fmt::Display for TruthRecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::EmptyId => "context record id must not be empty",
            Self::IdTooLong => "context record id exceeds its byte bound",
            Self::InvalidIdCharacter => {
                "context record id permits only ASCII letters, digits, '.', '_', ':', and '-'"
            }
            Self::TooManyEvidenceRefs => "truth record carries too many evidence references",
            Self::InvalidEvidenceRef => "evidence reference is empty, oversized, or malformed",
            Self::TooManyParticipations => "manifest carries too many participation records",
            Self::InvalidParticipation => "manifest carries an inconsistent participation record",
            Self::ParticipationDetailTooLong => "participation detail exceeds its byte bound",
            Self::CountTooLarge => "truth-record count exceeds its bound",
            Self::ObservationIndexOutOfRange => {
                "observation index must be nonzero and inside the count bound"
            }
        })
    }
}

impl std::error::Error for TruthRecordError {}

pub(crate) fn id_is_valid(value: &str) -> Result<(), TruthRecordError> {
    if value.is_empty() {
        return Err(TruthRecordError::EmptyId);
    }
    if value.len() > CONTEXT_ID_MAX_BYTES {
        return Err(TruthRecordError::IdTooLong);
    }
    if !value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b':' | b'-'))
    {
        return Err(TruthRecordError::InvalidIdCharacter);
    }
    Ok(())
}

macro_rules! context_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            // Both paths are absolute: this macro expands in `wire.rs` as well
            // as here, and an unqualified name that happens to be in scope at
            // the definition site is not in scope at every expansion site.
            pub fn parse(value: &str) -> Result<Self, $crate::context_truth::TruthRecordError> {
                $crate::context_truth::id_is_valid(value)?;
                Ok(Self(value.to_owned()))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let raw = <std::borrow::Cow<'de, str>>::deserialize(d)?;
                Self::parse(raw.as_ref()).map_err(serde::de::Error::custom)
            }
        }

        // Identifiers are validated strings on the wire.
        impl specta::Type for $name {
            fn definition(types: &mut specta::Types) -> specta::datatype::DataType {
                <String as specta::Type>::definition(types)
            }
        }
    };
}

// Visible to `crate::wire`, which mints the run and candidate identifiers the
// lifecycle grammar needs. One macro rather than a second copy of the same
// validation: wire identifiers and truth-record identifiers ARE the same
// identifiers, and two definitions of one charset is how they stop being.
pub(crate) use context_id;

context_id!(
    /// One immutable resolved manifest.
    ManifestId
);
context_id!(
    /// The one release supplement attached to a manifest.
    ReleaseSupplementId
);
context_id!(
    /// One ordered observation supplement attached to a manifest.
    ObservationSupplementId
);
context_id!(
    /// The one finalization supplement attached to a manifest.
    FinalizationSupplementId
);

/// A small, semantically bounded count serialized as a JSON number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct RecordCount(u32);

impl RecordCount {
    pub fn new(value: u32) -> Result<Self, TruthRecordError> {
        if value > CONTEXT_RECORD_COUNT_MAX {
            return Err(TruthRecordError::CountTooLarge);
        }
        Ok(Self(value))
    }

    pub const fn value(self) -> u32 {
        self.0
    }
}

impl<'de> serde::Deserialize<'de> for RecordCount {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = u32::deserialize(d)?;
        Self::new(raw).map_err(serde::de::Error::custom)
    }
}

impl specta::Type for RecordCount {
    fn definition(types: &mut specta::Types) -> specta::datatype::DataType {
        <u32 as specta::Type>::definition(types)
    }
}

/// A nonzero, per-manifest observation position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct ObservationIndex(u32);

impl ObservationIndex {
    pub fn new(value: u32) -> Result<Self, TruthRecordError> {
        if value == 0 || value > CONTEXT_RECORD_COUNT_MAX {
            return Err(TruthRecordError::ObservationIndexOutOfRange);
        }
        Ok(Self(value))
    }

    pub const fn value(self) -> u32 {
        self.0
    }
}

impl<'de> serde::Deserialize<'de> for ObservationIndex {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = u32::deserialize(d)?;
        Self::new(raw).map_err(serde::de::Error::custom)
    }
}

impl specta::Type for ObservationIndex {
    fn definition(types: &mut specta::Types) -> specta::datatype::DataType {
        <u32 as specta::Type>::definition(types)
    }
}

/// Bounded evidence references stored inline with a truth record.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(transparent)]
pub struct EvidenceRefs(Vec<EvidenceId>);

impl EvidenceRefs {
    pub fn new(values: Vec<EvidenceId>) -> Result<Self, TruthRecordError> {
        if values.len() > CONTEXT_EVIDENCE_MAX_COUNT {
            return Err(TruthRecordError::TooManyEvidenceRefs);
        }
        if values
            .iter()
            .any(|value| id_is_valid(value.as_str()).is_err())
        {
            return Err(TruthRecordError::InvalidEvidenceRef);
        }
        Ok(Self(values))
    }

    pub fn as_slice(&self) -> &[EvidenceId] {
        &self.0
    }
}

impl<'de> serde::Deserialize<'de> for EvidenceRefs {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = Vec::<EvidenceId>::deserialize(d)?;
        Self::new(raw).map_err(serde::de::Error::custom)
    }
}

impl specta::Type for EvidenceRefs {
    fn definition(types: &mut specta::Types) -> specta::datatype::DataType {
        <Vec<EvidenceId> as specta::Type>::definition(types)
    }
}

/// Bounded, internally consistent participation records for one manifest.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(transparent)]
pub struct ParticipationRecords(Vec<Participation>);

impl ParticipationRecords {
    pub fn new(values: Vec<Participation>) -> Result<Self, TruthRecordError> {
        if values.len() > CONTEXT_PARTICIPATION_MAX_COUNT {
            return Err(TruthRecordError::TooManyParticipations);
        }
        for value in &values {
            value
                .validate()
                .map_err(|_| TruthRecordError::InvalidParticipation)?;
            if value
                .detail
                .as_ref()
                .is_some_and(|detail| detail.len() > CONTEXT_PARTICIPATION_DETAIL_MAX_BYTES)
            {
                return Err(TruthRecordError::ParticipationDetailTooLong);
            }
        }
        Ok(Self(values))
    }

    pub fn as_slice(&self) -> &[Participation] {
        &self.0
    }
}

impl<'de> serde::Deserialize<'de> for ParticipationRecords {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = Vec::<Participation>::deserialize(d)?;
        Self::new(raw).map_err(serde::de::Error::custom)
    }
}

impl specta::Type for ParticipationRecords {
    fn definition(types: &mut specta::Types) -> specta::datatype::DataType {
        <Vec<Participation> as specta::Type>::definition(types)
    }
}

crate::closed_token_enum! {
    /// The assurance level a completed Context run is eligible to claim.
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
    pub enum Assurance {
        /// Normal production assurance: immutable facts reconstruct what happened.
        Trace,
        /// Only pinned fixtures with controlled inputs and declared tolerances.
        Deterministic,
    }
}

impl Assurance {
    /// The token this variant carries in `gwk.context_finalization.assurance`.
    ///
    /// Written out rather than derived, because a `&'static str` cannot be
    /// borrowed out of a `serde_json::Value`. That makes these two literals a
    /// second spelling of the wire form the DDL CHECK is held to, so
    /// `the_assurance_token_is_the_wire_spelling` asserts them against serde
    /// instead of leaving the agreement to review.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "trace",
            Self::Deterministic => "deterministic",
        }
    }
}

/// The one immutable compiler output for a spawn attempt.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct ResolvedManifest {
    pub id: ManifestId,
    pub attempt_id: AttemptId,
    pub manifest_digest: Digest,
    pub route_digest: Digest,
    pub authority_digest: Digest,
    pub source_count: RecordCount,
    pub source_bytes: ByteCount,
    pub participations: ParticipationRecords,
    pub evidence_ids: EvidenceRefs,
    pub resolved_at: Timestamp,
}

/// The exact adapter-rendered material and tool schemas released to an engine.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct ReleaseSupplement {
    pub id: ReleaseSupplementId,
    pub manifest_id: ManifestId,
    pub rendered_digest: Digest,
    pub tool_schema_digest: Digest,
    pub rendered_bytes: ByteCount,
    pub tool_schema_count: RecordCount,
    pub evidence_ids: EvidenceRefs,
    pub released_at: Timestamp,
}

/// One ordered post-boundary fact and the visibility limit around it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct ObservationSupplement {
    pub id: ObservationSupplementId,
    pub manifest_id: ManifestId,
    pub observation_index: ObservationIndex,
    pub fact_digest: Digest,
    pub observed_bytes: ByteCount,
    pub visible_source_count: RecordCount,
    pub truncated: bool,
    pub evidence_ids: EvidenceRefs,
    pub observed_at: Timestamp,
}

/// The completed run's outputs, verification, lifecycle root, and assurance.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, specta::Type)]
#[serde(deny_unknown_fields)]
pub struct FinalizationSupplement {
    pub id: FinalizationSupplementId,
    pub manifest_id: ManifestId,
    pub output_digest: Digest,
    pub verification_digest: Digest,
    pub approval_count: RecordCount,
    pub observation_count: RecordCount,
    pub final_event_root: Digest,
    pub lifecycle_complete: bool,
    pub assurance: Assurance,
    pub evidence_ids: EvidenceRefs,
    pub finalized_at: Timestamp,
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_assurance_token_is_the_wire_spelling() {
        // `as_str` feeds the SQL bind and serde feeds the payload. Two
        // spellings of one value is how a CHECK constraint starts refusing
        // rows the type system called legal.
        assert_eq!(Assurance::ALL.len(), 2);
        for assurance in Assurance::ALL {
            let encoded = serde_json::to_value(assurance).expect("serializes");
            assert_eq!(
                encoded.as_str().expect("a string on the wire"),
                assurance.as_str(),
                "{assurance:?} serializes under a different name than it reports"
            );
        }
    }

    use std::any::TypeId;

    use crate::ids::{AttemptId, ByteCount, EvidenceId, Timestamp};

    use super::*;
    use crate::context_participation::{Participation, ParticipationReason};

    const HEX: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn digest() -> Digest {
        Digest::from_hex(HEX).expect("valid digest")
    }

    fn count(value: u32) -> RecordCount {
        RecordCount::new(value).expect("bounded count")
    }

    fn evidence() -> EvidenceRefs {
        EvidenceRefs::new(vec![EvidenceId::new("evidence-1")]).expect("bounded evidence")
    }

    fn participations() -> ParticipationRecords {
        ParticipationRecords::new(vec![Participation::excluded(
            digest(),
            ParticipationReason::BudgetCut,
        )])
        .expect("valid participation")
    }

    fn manifest() -> ResolvedManifest {
        ResolvedManifest {
            id: ManifestId::parse("manifest-1").expect("valid id"),
            attempt_id: AttemptId::new("attempt-1"),
            manifest_digest: digest(),
            route_digest: digest(),
            authority_digest: digest(),
            source_count: count(1),
            source_bytes: ByteCount::new(64),
            participations: participations(),
            evidence_ids: evidence(),
            resolved_at: Timestamp::new("2026-08-14T12:00:00Z"),
        }
    }

    #[test]
    fn truth_record_shapes_are_distinct_and_stage_is_implicit() {
        let ids = [
            TypeId::of::<ResolvedManifest>(),
            TypeId::of::<ReleaseSupplement>(),
            TypeId::of::<ObservationSupplement>(),
            TypeId::of::<FinalizationSupplement>(),
        ];
        for (index, id) in ids.iter().enumerate() {
            assert!(
                !ids[..index].contains(id),
                "truth record types must remain distinct"
            );
        }

        let value = serde_json::to_value(manifest()).expect("serialize manifest");
        let object = value.as_object().expect("record is an object");
        assert!(!object.contains_key("stage"));
        assert!(!object.contains_key("prompt"));
        assert!(!object.contains_key("body"));
    }

    #[test]
    fn all_four_records_round_trip_with_required_fields() {
        let manifest = manifest();
        let release = ReleaseSupplement {
            id: ReleaseSupplementId::parse("release-1").expect("valid id"),
            manifest_id: manifest.id.clone(),
            rendered_digest: digest(),
            tool_schema_digest: digest(),
            rendered_bytes: ByteCount::new(128),
            tool_schema_count: count(2),
            evidence_ids: evidence(),
            released_at: Timestamp::new("2026-08-14T12:00:01Z"),
        };
        let observation = ObservationSupplement {
            id: ObservationSupplementId::parse("observation-1").expect("valid id"),
            manifest_id: manifest.id.clone(),
            observation_index: ObservationIndex::new(1).expect("valid index"),
            fact_digest: digest(),
            observed_bytes: ByteCount::new(32),
            visible_source_count: count(1),
            truncated: false,
            evidence_ids: evidence(),
            observed_at: Timestamp::new("2026-08-14T12:00:02Z"),
        };
        let finalization = FinalizationSupplement {
            id: FinalizationSupplementId::parse("finalization-1").expect("valid id"),
            manifest_id: manifest.id.clone(),
            output_digest: digest(),
            verification_digest: digest(),
            approval_count: count(1),
            observation_count: count(1),
            final_event_root: digest(),
            lifecycle_complete: true,
            assurance: Assurance::Trace,
            evidence_ids: evidence(),
            finalized_at: Timestamp::new("2026-08-14T12:00:03Z"),
        };

        let values = [
            serde_json::to_value(&manifest).expect("manifest"),
            serde_json::to_value(&release).expect("release"),
            serde_json::to_value(&observation).expect("observation"),
            serde_json::to_value(&finalization).expect("finalization"),
        ];
        assert_eq!(values.len(), 4);
        assert_eq!(
            serde_json::from_value::<ResolvedManifest>(values[0].clone()).expect("decode"),
            manifest
        );
        assert_eq!(
            serde_json::from_value::<ReleaseSupplement>(values[1].clone()).expect("decode"),
            release
        );
        assert_eq!(
            serde_json::from_value::<ObservationSupplement>(values[2].clone()).expect("decode"),
            observation
        );
        assert_eq!(
            serde_json::from_value::<FinalizationSupplement>(values[3].clone()).expect("decode"),
            finalization
        );
    }

    #[test]
    fn bounds_fail_closed_at_construction_and_decode() {
        assert_eq!(
            ManifestId::parse(&"x".repeat(CONTEXT_ID_MAX_BYTES + 1)),
            Err(TruthRecordError::IdTooLong)
        );
        assert_eq!(
            ManifestId::parse("manifest/escape"),
            Err(TruthRecordError::InvalidIdCharacter)
        );
        assert_eq!(
            RecordCount::new(CONTEXT_RECORD_COUNT_MAX + 1),
            Err(TruthRecordError::CountTooLarge)
        );
        assert_eq!(
            ObservationIndex::new(0),
            Err(TruthRecordError::ObservationIndexOutOfRange)
        );
        assert_eq!(
            EvidenceRefs::new(
                (0..=CONTEXT_EVIDENCE_MAX_COUNT)
                    .map(|index| EvidenceId::new(format!("evidence-{index}")))
                    .collect()
            ),
            Err(TruthRecordError::TooManyEvidenceRefs)
        );
        assert_eq!(
            ParticipationRecords::new(vec![
                Participation::excluded(digest(), ParticipationReason::BudgetCut,)
                    .with_detail("x".repeat(CONTEXT_PARTICIPATION_DETAIL_MAX_BYTES + 1))
            ]),
            Err(TruthRecordError::ParticipationDetailTooLong)
        );

        let mut value = serde_json::to_value(manifest()).expect("serialize");
        value["source_count"] = serde_json::json!(CONTEXT_RECORD_COUNT_MAX + 1);
        assert!(serde_json::from_value::<ResolvedManifest>(value).is_err());
    }

    #[test]
    fn unknown_fields_and_invalid_nested_participation_are_refused() {
        let mut unknown = serde_json::to_value(manifest()).expect("serialize");
        unknown["stage"] = serde_json::json!("resolved");
        assert!(serde_json::from_value::<ResolvedManifest>(unknown).is_err());

        let mut invalid = serde_json::to_value(manifest()).expect("serialize");
        invalid["participations"][0]["state"] = serde_json::json!("excluded");
        invalid["participations"][0]
            .as_object_mut()
            .expect("participation object")
            .remove("reason");
        assert!(serde_json::from_value::<ResolvedManifest>(invalid).is_err());
    }

    #[test]
    fn assurance_tokens_are_closed_and_explicit() {
        assert_eq!(
            serde_json::to_string(&Assurance::Trace).expect("serialize"),
            "\"trace\""
        );
        assert_eq!(
            serde_json::to_string(&Assurance::Deterministic).expect("serialize"),
            "\"deterministic\""
        );
        assert!(serde_json::from_str::<Assurance>("\"ordinary_replay\"").is_err());
        // Macro-derived, so a third level moves this count; the DDL parity gate
        // in `xtask/src/contract.rs` holds the SQL CHECK to the same list.
        assert_eq!(Assurance::ALL.len(), 2);
    }

    #[test]
    fn the_participation_count_bound_is_enforced() {
        // Nothing failed when this bound was removed: every prior test built a
        // handful of rows, so `values.len() > CONTEXT_PARTICIPATION_MAX_COUNT`
        // was an arm no input reached. This is the input — one past the bound
        // refuses, the bound itself is legal, and deleting the check in
        // `ParticipationRecords::new` reds exactly here.
        let over: Vec<Participation> = (0..=CONTEXT_PARTICIPATION_MAX_COUNT)
            .map(|_| Participation::active(digest()))
            .collect();
        assert_eq!(over.len(), CONTEXT_PARTICIPATION_MAX_COUNT + 1);
        assert_eq!(
            ParticipationRecords::new(over),
            Err(TruthRecordError::TooManyParticipations)
        );

        let at_bound: Vec<Participation> = (0..CONTEXT_PARTICIPATION_MAX_COUNT)
            .map(|_| Participation::active(digest()))
            .collect();
        assert!(ParticipationRecords::new(at_bound).is_ok());
    }

    #[test]
    fn the_evidence_id_validity_sweep_is_enforced() {
        // The count bound had a test; the per-id VALIDITY sweep did not, so
        // removing the `id_is_valid` walk in `EvidenceRefs::new` left every
        // test green while a `manifest/escape` id rode into a truth record.
        // Both the construction path and the wire path must refuse it.
        assert_eq!(
            EvidenceRefs::new(vec![EvidenceId::new("evidence/escape")]),
            Err(TruthRecordError::InvalidEvidenceRef)
        );
        assert!(serde_json::from_str::<EvidenceRefs>(r#"["evidence/escape"]"#).is_err());
        // The positive control, so the refusals above cannot pass by the whole
        // shape failing to parse.
        assert!(serde_json::from_str::<EvidenceRefs>(r#"["evidence-1"]"#).is_ok());
    }
}
